//! Times diff collection against a real tree:
//!     cargo run --example probe-diff -- <repo>
use reviewpad::git::Repository;
use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let started = Instant::now();
    let repository = Repository::discover(std::path::Path::new(&path)).expect("not a repo");
    let discover = started.elapsed();

    let started = Instant::now();
    let diff = repository.diff().expect("diff failed");
    let collect = started.elapsed();

    let rows: usize = diff.files.iter().map(|file| file.lines.len()).sum();
    let listed = diff.files.iter().filter(|file| file.is_media()).count();
    println!("discover  {discover:>8.0?}");
    println!("diff      {collect:>8.0?}");
    println!(
        "files     {:>8}  ({listed} listed without a patch)",
        diff.files.len()
    );
    println!("rows      {rows:>8}");

    let mut heaviest: Vec<_> = diff
        .files
        .iter()
        .map(|file| (file.lines.len(), file.path.as_str()))
        .collect();
    heaviest.sort_unstable();
    heaviest.reverse();
    println!("heaviest files:");
    for (rows, path) in heaviest.iter().take(5) {
        println!("  {rows:>7}  {path}");
    }
}
