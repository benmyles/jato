mod store;

use anyhow::{Result, bail};
use store::{Store, cache_limit, parse_size};

fn main() {
    if let Err(e) = run() {
        eprintln!("jato: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("stats") if args.len() == 1 || args[1..] == ["--json"] => {
            let stats = Store::open()?.stats()?;
            if args.len() == 2 {
                println!("{}", serde_json::to_string_pretty(&stats)?);
            } else {
                println!("Cache     {}", stats.directory.display());
                println!(
                    "Entries   {} ({} unique chunks)",
                    stats.entries, stats.chunks
                );
                println!(
                    "Storage   {} charged bytes / {} budget",
                    stats.charged_bytes, stats.limit_bytes
                );
                println!(
                    "Payload   {} compressed bytes for {} artifact bytes",
                    stats.compressed_bytes, stats.artifact_bytes
                );
                for (name, value) in stats.counters {
                    println!("{name:<20} {value}");
                }
            }
        }
        Some("gc") if args.len() <= 2 => {
            let limit = args
                .get(1)
                .map(|s| parse_size(s))
                .transpose()?
                .unwrap_or(cache_limit()?);
            println!("Evicted {} entries", Store::open()?.prune(limit)?);
        }
        _ => bail!("usage: jato stats [--json] | gc [SIZE]"),
    }
    Ok(())
}
