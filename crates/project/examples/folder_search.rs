//! Runs the editor's folder search once and prints every result, sorted, with
//! the totals and the time taken, so two builds can be compared exactly.
//!
//! Usage: folder_search <folder> <lw words...>   (for example: -i -t rust unwrap)
use linework_project::{search, SearchRequest};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut request = SearchRequest::new(args[1].clone().into(), args[2..].to_vec());
    request.max_hits = 50_000_000;
    let started = Instant::now();
    let report = search(&request, &AtomicBool::new(false)).expect("search");
    let took = started.elapsed().as_secs_f64();
    let mut rows: Vec<String> = report
        .hits
        .iter()
        .map(|h| format!("{}\t{}\t{}\t{}\t{}\t{}", h.path.display(), h.line, h.column, h.offset, h.length, h.preview))
        .collect();
    rows.sort();
    for row in &rows {
        println!("{row}");
    }
    let mut errors = report.errors.clone();
    errors.sort();
    for error in &errors {
        println!("ERROR\t{error}");
    }
    eprintln!(
        "{} results, {} files searched, {} binary skipped, {} errors, truncated {}, {:.3} s",
        report.hits.len(),
        report.files_scanned,
        report.binary_skipped,
        report.errors.len(),
        report.truncated,
        took
    );
}
