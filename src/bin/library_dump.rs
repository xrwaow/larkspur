//! Diagnostic binary: scan directories and print the resulting library —
//! playlists and their songs — with no window. The library equivalent of
//! `layout_dump`: a way to "see" what a scan produced without the GUI.

use std::path::PathBuf;

use larkspur::model::{Library, LibraryCache};

fn main() {
    let roots: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    if roots.is_empty() {
        eprintln!("usage: library_dump <dir> [more dirs...]");
        std::process::exit(2);
    }

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

    println!("songs: {}", library.songs.len());

    // Nominal bitrates are normalized to bits per second on load; this is the
    // quickest sanity check that the unit conversion is right.
    let nominal: Vec<u32> = library.songs.values().filter_map(|s| s.nominal_bitrate).collect();
    if !nominal.is_empty() {
        let min = nominal.iter().min().unwrap();
        let max = nominal.iter().max().unwrap();
        let avg = nominal.iter().map(|&b| b as u64).sum::<u64>() / nominal.len() as u64;
        println!("nominal bitrate: min {min}  avg {avg}  max {max} bps");
    }

    println!("playlists: {}", library.playlists.len());
    for playlist in library.playlists() {
        let kind = if playlist.is_custom() { "custom" } else { "auto" };
        println!(
            "\n  [{kind}] {} — {} song(s)",
            playlist.meta.title,
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
}
