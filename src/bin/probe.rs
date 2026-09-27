//! Dev diagnostics in one binary: seek accuracy, live-bitrate profiling, and
//! library scans, all without a window.
//!
//!     probe seek <file>          — exercise the decode + seek chain
//!     probe bitrate <file>       — print the live-bitrate profile
//!     probe library <dirs...>    — scan directories and print the library
//!     probe layout               — print the resolved UI layout as a diagram

use std::path::{Path, PathBuf};
use std::time::Duration;

use rodio::Source;

use larkspur::analysis::analyze_track;
use larkspur::audio::build_source;
use larkspur::model::{Library, LibraryCache, SongMetadata};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("seek") if args.len() == 2 => seek(&args[1]),
        Some("bitrate") if args.len() == 2 => bitrate(&args[1]),
        Some("library") if args.len() > 1 => library(&args[1..]),
        Some("layout") if args.len() == 1 => {
            print!("{}", larkspur::ui::layout::describe_app());
            Ok(())
        }
        _ => {
            eprintln!("usage: probe seek|bitrate <file>");
            eprintln!("       probe library <dir> [more dirs...]");
            eprintln!("       probe layout");
            std::process::exit(2);
        }
    }
}

/// Throwaway diagnostic: verifies the exact decode + seek chain the app uses,
/// without needing a UI.
fn seek(path: &String) -> anyhow::Result<()> {
    let path = Path::new(path);

    // Exercise the tag-reading path too, like the app's track_parts.
    let metadata = SongMetadata::load(path)?;
    println!(
        "metadata: {:?} / {:?}, duration {:?}",
        &metadata.song_name,
        &metadata.artists,
        metadata.duration,
    );

    let mut source = build_source(path)?;
    println!("total_duration: {:?}", source.total_duration());
    println!("channels: {}, sample rate: {}", source.channels(), source.sample_rate());

    // Consume ~1s of samples so we're mid-stream, like the app at 0:18.
    let mut decoded = 0usize;
    let target = source.sample_rate().get() as usize * source.channels().get() as usize;
    while decoded < target {
        match source.next() {
            Some(_) => decoded += 1,
            None => break,
        }
    }
    println!("decoded {decoded} samples (~1s)");

    for target in [Duration::from_secs(10), Duration::from_secs(20)] {
        match source.try_seek(target) {
            Ok(()) => {
                // Confirm samples still flow after the seek.
                let flowed = (0..1000).any(|_| source.next().is_some());
                println!("try_seek({target:?}) -> Ok, samples flow after seek: {flowed}");
            }
            Err(e) => println!("try_seek({target:?}) -> ERR: {e}"),
        }
    }

    // Seek back to the very start, then past the end.
    for target in [Duration::from_secs(0), Duration::from_secs(179)] {
        match source.try_seek(target) {
            Ok(()) => println!("try_seek({target:?}) -> Ok"),
            Err(e) => println!("try_seek({target:?}) -> ERR: {e}"),
        }
    }

    // Determinism check: two fresh linear decodes must agree exactly.
    let mut a = build_source(path)?;
    let mut b = build_source(path)?;
    let da: Vec<f32> = (0..8192).map(|_| a.next().unwrap_or(0.0)).collect();
    let db: Vec<f32> = (0..8192).map(|_| b.next().unwrap_or(0.0)).collect();
    println!("linear-vs-linear identical: {}", da == db);

    // Accuracy check: samples after seek(10s) must align with the samples
    // at the 10s mark from a plain linear decode. Exact equality is too
    // strict for opus — libopus's overlap-add blends the first packet
    // after a seek with stale decoder state — so compare by RMS error.
    let mut source = build_source(path)?;
    let mut seen = 0usize;
    let frame = source.sample_rate().get() as usize * source.channels().get() as usize;
    let mut linear_window = Vec::new();
    while let Some(s) = source.next() {
        if seen >= frame * 10 && linear_window.len() < 8192 {
            linear_window.push(s);
        }
        if linear_window.len() == 8192 {
            break;
        }
        seen += 1;
    }
    source
        .try_seek(Duration::from_secs(10))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let seeked_window: Vec<f32> = (0..8192).filter_map(|_| source.next()).collect();
    let n = seeked_window.len().min(linear_window.len());
    let rms = (seeked_window[..n]
        .iter()
        .zip(&linear_window[..n])
        .map(|(a, b)| {
            let d = a - b;
            (d * d) as f64
        })
        .sum::<f64>()
        / n as f64)
        .sqrt();
    println!("seek(10s) vs linear RMS error: {rms:.6} ({} samples)", n);

    Ok(())
}

/// Print the live-bitrate profile — the numbers the UI shows while a track
/// plays — obtained via the shared one-pass analysis.
fn bitrate(path: &String) -> anyhow::Result<()> {
    let path = Path::new(path);
    let bucket = Duration::from_millis(250);
    let analysis = analyze_track(path, 120, bucket)?;
    let profile = &analysis.bitrate;

    let buckets: Vec<u32> = profile.bitrates().into_iter().filter(|&b| b > 0).collect();
    if buckets.is_empty() {
        println!("{}: no packets", path.display());
        return Ok(());
    }

    let min = *buckets.iter().min().unwrap();
    let max = *buckets.iter().max().unwrap();
    let avg = buckets.iter().map(|&b| b as u64).sum::<u64>() / buckets.len() as u64;

    println!("{}", path.display());
    println!(
        "  {} buckets of {:?} — min {min}  avg {avg}  max {max} bps",
        buckets.len(),
        bucket
    );
    print!("  first 8: ");
    for bps in buckets.iter().take(8) {
        print!("{bps} ");
    }
    println!();

    Ok(())
}

/// Scan directories and print the resulting library — playlists and their
/// songs — without the GUI.
fn library(roots: &[String]) -> anyhow::Result<()> {
    let roots: Vec<PathBuf> = roots.iter().map(PathBuf::from).collect();

    // A throwaway cache, so this never touches the real library.
    let cache_path =
        std::env::temp_dir().join(format!("larkspur-library-dump-{}.json", std::process::id()));
    let mut cache = LibraryCache::open(cache_path.clone());
    let mut library = Library::default();
    let report = cache.sync(&mut library, &roots, true);

    println!("roots: {}", roots.len());
    println!(
        "scan: parsed {}  reused {}  removed {}  errors {}",
        report.parsed,
        report.reused,
        report.removed,
        report.errors.len()
    );
    for (path, err) in report.errors.iter().take(5) {
        println!("  ! {}: {err}", path.display());
    }

    println!("songs: {}", library.song_count());

    // Nominal bitrates are normalized to bits per second on load; this is the
    // quickest sanity check that the unit conversion is right.
    let nominal: Vec<u32> = library.songs().filter_map(|s| s.nominal_bitrate).collect();
    if !nominal.is_empty() {
        let min = nominal.iter().min().unwrap();
        let max = nominal.iter().max().unwrap();
        let avg = nominal.iter().map(|&b| b as u64).sum::<u64>() / nominal.len() as u64;
        println!("nominal bitrate: min {min}  avg {avg}  max {max} bps");
    }

    println!("playlists: {}", library.playlists().len());
    for playlist in library.playlists() {
        let kind = if playlist.is_custom() { "custom" } else { "auto" };
        println!(
            "\n  [{kind}] {} — {} song(s)",
            playlist.title(),
            playlist.len()
        );
        for song in library.songs_of(playlist.id).iter().take(4) {
            println!(
                "      {} — {}",
                song.primary_artist(),
                song.song_name.as_deref().unwrap_or("(untitled)")
            );
        }
    }

    let artists = library.artists();
    println!("\nartists: {}", artists.len());
    for artist in artists.iter().take(10) {
        println!(
            "  {artist} → {} album playlist(s)",
            library.discography(artist).len()
        );
    }

    let _ = std::fs::remove_file(cache_path);

    Ok(())
}
