//! Diagnostic: what the engine sees about a volume, and which write mode it
//! will therefore pick.
//!
//! Useful when a download is slower than expected on a particular drive, since
//! the answer is usually that the sparse path was unavailable and the engine
//! fell back to the bounded-window mode.
//!
//!   cargo run --example volprobe -- C:\ D:\

use std::path::Path;

use idm_core::win::volume;

fn main() {
    let targets: Vec<String> = std::env::args().skip(1).collect();
    let targets: Vec<&str> =
        if targets.is_empty() { vec!["."] } else { targets.iter().map(String::as_str).collect() };

    for target in targets {
        let path = Path::new(target);
        println!("{target}");

        match volume::caps_for(path) {
            Ok(caps) => {
                println!("  filesystem       {}", caps.filesystem);
                println!("  volume root      {}", caps.root.display());
                println!("  fixed disk       {}", caps.is_fixed);
                println!("  sparse supported {}", caps.supports_sparse);
                println!(
                    "  quotas           supported={} restricted={}",
                    caps.supports_quotas, caps.quota_restricted
                );
                println!(
                    "  -> sparse path   {}",
                    if caps.should_try_sparse() {
                        "yes (fast: no NTFS zero-fill)"
                    } else {
                        "no (falls back to a bounded write window)"
                    }
                );
            }
            Err(error) => println!("  caps unavailable: {error}"),
        }

        match volume::disk_space(path) {
            Ok(space) => {
                let gib = |bytes: u64| bytes as f64 / (1u64 << 30) as f64;
                println!(
                    "  space            {:.1} GiB free to you / {:.1} GiB free / {:.1} GiB total",
                    gib(space.available_to_caller),
                    gib(space.total_free),
                    gib(space.total)
                );
            }
            Err(error) => println!("  space unavailable: {error}"),
        }

        match volume::has_seek_penalty(path) {
            Ok(Some(true)) => println!("  seek penalty     yes (spinning disk: one writer shard)"),
            Ok(Some(false)) => println!("  seek penalty     no (SSD/NVMe: shards may scale)"),
            Ok(None) => println!("  seek penalty     unknown (assumed yes)"),
            Err(error) => println!("  seek penalty     unavailable: {error}"),
        }
        println!();
    }
}
