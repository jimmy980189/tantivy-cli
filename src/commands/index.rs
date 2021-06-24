use chan;
use clap::value_t;
use clap::ArgMatches;
use std::cmp;
use std::convert::From;
use std::fs::File;
use std::io;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::path::PathBuf;
use std::thread;
use tantivy;
use tantivy::merge_policy::NoMergePolicy;
use tantivy::Document;
use tantivy::Index;
use tantivy::IndexWriter;
use time::Instant;
use tantivy::tokenizer::*;
use tantivy_jieba;

pub fn run_index_cli(argmatch: &ArgMatches) -> Result<(), String> {
    let index_directory = PathBuf::from(argmatch.value_of("index").unwrap());
    let document_source = argmatch
        .value_of("file")
        .map(|path| DocumentSource::FromFile(PathBuf::from(path)))
        .unwrap_or(DocumentSource::FromPipe);
    let no_merge = argmatch.is_present("nomerge");
    let mut num_threads = value_t!(argmatch, "num_threads", usize)
        .map_err(|_| format!("Failed to read num_threads argument as an integer."))?;
    if num_threads == 0 {
        num_threads = 1;
    }
    let buffer_size: usize = value_t!(argmatch, "memory_size", usize)
        .map_err(|_| format!("Failed to read the buffer size argument as an integer."))?;
    let buffer_size_per_thread = buffer_size / num_threads;
    run_index(
        index_directory,
        document_source,
        buffer_size_per_thread,
        num_threads,
        no_merge,
    )
    .map_err(|e| format!("Indexing failed : {:?}", e))
}

//noinspection RsExternalLinter
fn run_index(
    directory: PathBuf,
    document_source: DocumentSource,
    buffer_size_per_thread: usize,
    num_threads: usize,
    no_merge: bool,
) -> tantivy::Result<()> {
    let my_tokenizer = tantivy_jieba::JiebaTokenizer {};
    let index = Index::open_in_dir(&directory)?;
    index
        .tokenizers()
        .register("jieba", my_tokenizer);
    let schema = index.schema();
    let (line_sender, line_receiver) = chan::sync(10_000);
    let (doc_sender, doc_receiver) = chan::sync(10_000);

    thread::spawn(move || {
        let articles = document_source.read().unwrap();
        for article_line_res in articles.lines() {
            let article_line = article_line_res.unwrap();
            line_sender.send(article_line);
        }
    });

    let num_threads_to_parse_json = cmp::max(1, num_threads / 4);
    log::info!("Using {} threads to parse json", num_threads_to_parse_json);
    for _ in 0..num_threads_to_parse_json {
        let schema_clone = schema.clone();
        let doc_sender_clone = doc_sender.clone();
        let line_receiver_clone = line_receiver.clone();
        thread::spawn(move || {
            for article_line in line_receiver_clone {
                match schema_clone.parse_document(&article_line) {
                    Ok(doc) => {
                        doc_sender_clone.send(doc);
                    }
                    Err(err) => {
                        println!("Failed to add document doc {:?}", err);
                    }
                }
            }
        });
    }
    drop(doc_sender);

    let mut index_writer = if num_threads > 0 {
        index.writer_with_num_threads(num_threads, buffer_size_per_thread)
    } else {
        index.writer(buffer_size_per_thread)
    }?;

    if no_merge {
        index_writer.set_merge_policy(Box::new(NoMergePolicy));
    }

    let start_overall = Instant::now();
    let index_result = index_documents(&mut index_writer, doc_receiver);
    {
        let duration = start_overall - Instant::now();
        log::info!("Indexing the documents took {} s", duration.whole_seconds());
    }

    match index_result {
        Ok(docstamp) => {
            println!("Commit succeed, docstamp at {}", docstamp);
            println!("Waiting for merging threads");
            index_writer.wait_merging_threads()?;
            println!("Terminated successfully!");
            {
                let duration = start_overall - Instant::now();
                log::info!(
                    "Indexing the documents took {} s overall (indexing + merge)",
                    duration.whole_seconds()
                );
            }
            Ok(())
        }
        Err(e) => {
            println!("Error during indexing, rollbacking.");
            index_writer.rollback().unwrap();
            println!("Rollback succeeded");
            Err(e)
        }
    }
}

fn index_documents(
    index_writer: &mut IndexWriter,
    doc_receiver: chan::Receiver<Document>,
) -> tantivy::Result<u64> {
    let group_count = 100_000;
    let mut num_docs = 0;
    let cur = Instant::now();
    for doc in doc_receiver {
        index_writer.add_document(doc);
        if num_docs > 0 && (num_docs % group_count == 0) {
            println!("{} Docs", num_docs);
            let new = Instant::now();
            let elapsed = new - cur;
            println!(
                "{:.0} docs / hour",
                num_docs as f32 * 3600.0 * 1_000_000.0 as f32
                    / (elapsed.whole_microseconds() as f32)
            );
        }
        num_docs += 1;
    }
    index_writer.commit()
}

enum DocumentSource {
    FromPipe,
    FromFile(PathBuf),
}

impl DocumentSource {
    fn read(&self) -> io::Result<BufReader<Box<dyn Read>>> {
        Ok(match self {
            &DocumentSource::FromPipe => BufReader::new(Box::new(io::stdin())),
            &DocumentSource::FromFile(ref filepath) => {
                let read_file = File::open(&filepath)?;
                BufReader::new(Box::new(read_file))
            }
        })
    }
}
