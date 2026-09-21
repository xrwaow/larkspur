//! Diagnostic binary: demux a file and print its live-bitrate profile — the
//! numbers the UI shows while a track plays — with no window. The bitrate
//! equivalent of `seek_probe`.

use std::path::PathBuf;
use std::time::Duration;

use larkspur::bitrate::BitrateProfile;

fn main() {
    let Some(arg) = std::env::args().nth(1) else {
        eprintln!("usage: bitrate_probe <song>");
        std::process::exit(2);
    };
    let path = PathBuf::from(arg);
    let bucket = Duration::from_millis(250);

    let profile = match BitrateProfile::compute(&path, bucket) {
        Ok(profile) => profile,
        Err(e) => {
            eprintln!("failed to profile {}: {e}", path.display());
            std::process::exit(1);
        }
    };

    let buckets: Vec<u32> = profile.buckets().iter().copied().filter(|&b| b > 0).collect();
    if buckets.is_empty() {
        println!("{}: no packets", path.display());
        return;
    }

    let min = *buckets.iter().min().unwrap();
    let max = *buckets.iter().max().unwrap();
    let avg = buckets.iter().map(|&b| b as u64).sum::<u64>() / buckets.len() as u64;

    println!("{}", path.display());
    println!(
        "  {} buckets of {:?} — min {min}  avg {avg}  max {max} bps",
        profile.buckets().len(),
        bucket
    );
    print!("  first 8: ");
    for bps in profile.buckets().iter().take(8) {
        print!("{bps} ");
    }
    println!();
}
