//! Prints where the editor's reference index finds a name defined and used,
//! as `defs` or `uses` lines `path:line`, sorted, for comparing with lw.
//!
//! Usage: name_index <folder> <name>...
use std::sync::atomic::AtomicBool;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let index = linework_project::index(std::path::Path::new(&args[1]), &AtomicBool::new(false));
    let mut rows = Vec::new();
    for name in &args[2..] {
        for symbol in index.definitions(name) {
            rows.push(format!("defs\t{name}\t{}:{}", symbol.path.display(), symbol.line));
        }
        for symbol in index.references(name) {
            rows.push(format!("uses\t{name}\t{}:{}", symbol.path.display(), symbol.line));
        }
    }
    rows.sort();
    rows.dedup();
    for row in rows {
        println!("{row}");
    }
}
