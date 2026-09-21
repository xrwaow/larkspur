# Larkspur

A fast, native music player for Linux, written in Rust.

Built around a headless, decoupled audio core (Symphonia + rodio) and a
GPU-accelerated, modular UI (GPUI) where panels — now playing, lyrics,
queue, live bitrate meter, spectrum, etc. — are independent, addable/
removable modules reading from shared, centralized state rather than
copies of it.

## Goals

- Native Linux, no Electron/web-view overhead.
- 120+ fps UI animations without dropping frames during playback.
- First-class Opus support, alongside FLAC and MP3.
- Live, real-time bitrate display (not just a file's declared/average
  bitrate).
- Lyrics support — both plain embedded lyrics and synced (`.lrc`-style,
  timestamped) lyrics.
- A UI built from independent modules that can be added, removed, and
  rearranged, not a monolithic fixed layout.

## Stack

| Concern                  | Crate                          | Why                                                                 |
|---------------------------|---------------------------------|----------------------------------------------------------------------|
| Audio decode              | `symphonia`                    | Pure-Rust decoder/demuxer. FLAC and Opus are default-enabled; MP3 requires the `mp3` feature flag (licensing, not technical, reasons). Also the packet-level source for live bitrate calculation. |
| Audio output               | `rodio` (Symphonia backend)     | Higher-level playback API over `cpal`, avoids maintaining a second decode path. |
| Tag / metadata reading     | `lofty`                         | Reads title/artist/album/track/lyrics tags and embedded cover art across FLAC/MP3/Opus/OGG containers. |
| UI framework                | `gpui`                          | GPU-accelerated, retained-mode. `Entity<T>` + `Render` views map naturally onto "shared state + independent subscriber modules." Standalone crate, Linux (Wayland + X11) supported. |
| Cover cache eviction        | `lru`                           | Byte-budgeted in-memory LRU for decoded cover pixel buffers. |
| Song identity hashing        | `twox-hash`                     | Stable, documented hash algorithm — deliberately **not** `std`'s `DefaultHasher` (SipHash), which is not guaranteed stable across Rust versions and would silently break a persisted library/cover cache on toolchain upgrades. |
| Errors                       | `anyhow` / `thiserror`          | App-level vs. library-level error handling split. |

## Source map

Paths are relative to this crate's root. This is the fastest way in:

| Path | What lives there |
|------|------------------|
| `src/lib.rs` | Crate roots — `audio`, `bitrate`, `decode`, `model`, `opus`, `waveform`, `ui`. |
| `src/model/` | The whole data schema, split by concern: `identity` (`SongId` + hashing), `song` (`SongMetadata` + lofty loading), `lyrics` (`Lyrics`/`LyricLine` + `parse_lrc`), `cover` (`CoverState`/`DecodedImage`/`CoverCache`), `library` (`Album`/`Library`), `streaming` (`StreamingInfo` + `SongStatus`), `input` (`InputAction`), `config` (`Config`). Framework-agnostic — no GPUI, no rodio — so it unit-tests without a device or a window. |
| `src/audio.rs` | `PlaybackController` — owns the rodio `Player` and device sink, the `Vec<PathBuf>` queue, seek/next/prev, drain-detection auto-advance (`tick_advance`), and the current track's metadata/duration. `track_parts` is the single place decode source, tags, and duration are chosen. Deliberately does **not** own the waveform. |
| `src/decode.rs` | `open_track(path, gapless)` — the one place a Symphonia container is opened and a playable track selected. Shared by `opus.rs` and `waveform.rs`. |
| `src/opus.rs` | `OpusSource` — symphonia's Ogg demuxer feeding libopus, because symphonia 0.5 demuxes Opus but has no decoder. Handles gapless pre-skip/end trims and accurate seek (decode-and-drop to the exact timestamp). |
| `src/waveform.rs` | `compute_waveform(path, buckets)` — decodes to per-512-sample peaks, then buckets and normalizes them. Opus routes through `OpusSource`. |
| `src/bitrate.rs` | `BitrateTracker` — rolling live-bitrate estimation from decoded packet sizes (the thing the old `test.rs` proved out). Unit-tested and driven by caller-supplied media time. Not yet wired into playback. |
| `src/ui/mod.rs` | UI module roots — `container`, `cover`, `introspect`, `layout`, `playback`, `state`. |
| `src/ui/main.rs` | The `larkspur_ui` binary: window setup + binding live views to the shared state. The layout itself lives in `src/ui/layout.rs`. |
| `src/ui/state.rs` | `PlaybackState` — the shared, observable playback `Entity`. Owns the controller, the async waveform load, and the single ticker; views observe it instead of polling. |
| `src/ui/container.rs` | The layout system: `Container` trait, `LayoutPlan`/`Layout`/`Child`/`Size`, `Dock`/`Edge`/`Align`, `Workspace` (module registry + plan), `EmptyView` placeholder. |
| `src/ui/layout.rs` | `app_layout()` — the one definition of the dock/center arrangement, shared by the real UI and the introspection tools. |
| `src/ui/introspect.rs` | Resolves a `LayoutPlan` into rectangles and renders it as an ASCII diagram + dock legend + border/adjacency list. Pure geometry, no GPUI. |
| `src/ui/cover.rs` | `CoverView` — the 240 px now-playing cover square (placeholder rendering for now). |
| `src/ui/playback.rs` | `PlaybackView` — transport buttons, seek bar + waveform (`bars`/`line`), time labels, and key handling (`←`/`→` seek, `space` play/pause) routed through `InputAction`. |
| `src/bin/seek_probe.rs` | Diagnostic binary: runs the real decode + seek chain headlessly and reports seek-vs-linear RMS error. |
| `src/bin/layout_dump.rs` | Prints the resolved UI layout as text — no window needed. |
| `tests/layout.rs` | Layout tests over the introspection tooling (see `tests/README.md`). |

## Running

```sh
cargo run --bin larkspur_ui -- song1.flac song2.opus   # the GUI
cargo run --bin seek_probe  -- song.opus               # decode/seek check, no UI
cargo run --bin layout_dump                            # the UI layout as text
cargo test                                             # model + layout tests
```

## Architecture

```
                 ┌─────────────────────┐
                 │   Audio thread       │
                 │ (Symphonia decode +  │
                 │  rodio playback +    │
                 │  live bitrate calc)  │
                 └──────────┬───────────┘
                             │ events / channel
                 ┌──────────▼───────────┐
                 │  Entity<Library>      │   ← single source of truth
                 │  Entity<StreamingInfo>│      for static + live state
                 └──────────┬───────────┘
                             │ subscriptions
        ┌──────────┬─────────┼─────────┬───────────┐
        ▼          ▼          ▼          ▼           ▼
  NowPlaying   Lyrics    BitrateMeter  Queue    (your next module)
   (View)      (View)      (View)      (View)        (View)
```

The audio thread never owns UI state and the UI never blocks on
decode — they only communicate through the shared `Entity` state.
Modules are just `Render` impls that read a slice of that state; a
"workspace" view holds the list of currently-active module IDs, so
adding/removing a module is a list mutation, not a rewrite of the
others.

> **Mostly wired now.** `PlaybackState` (an `Entity`) owns the controller
> and is shared with the views, which observe it rather than polling — so
> adding a playback-reading module is a subscription, not another timer.
> `Library` and `StreamingInfo` still exist only as model types (no
> `Entity` yet), and the lyrics/bitrate/queue modules don't exist — the
> placeholder panels in the layout below are where they'll go.

### UI layout (current)

`ui/layout.rs` defines the workspace from edge docks plus a center,
resolved by `ui/container.rs`; `ui/main.rs` only binds live views to its
module ids. Run `cargo run --bin layout_dump` to see it as a diagram:

| Dock | Module(s) | Band size | `z` |
|------|-----------|-----------|-----|
| bottom | `playback` (transport bar) | content height | 1 |
| right | `empty-above-cover` (fills) + `cover` | 240 px | 2 |
| left | `empty-left` | 240 px | 2 |
| center | `empty` | — | — |

`Workspace::render_dock_stack` applies the highest `z` first: it hugs
the window edge, and lower-`z` docks fit into what remains. Side docks
(`left`/`right`) carry a −1 handicap against top/bottom docks, which is
exactly why the two 240 px rails need `z = 2` — it lets them span the
full window height and puts the bottom bar *between* them, keeping the
transport bar centered in the window instead of shifted left by the
cover. Without the `z`, the bottom bar wins the full width and the
rails sit above it instead of beside it.

Because `Container::container_id` is a per-type constant, a placeholder
that has to fill several sections is registered per-slot with
`Workspace::push_as(id, entity)`; each slot still gets its own
`EmptyView` entity, since `AnyView`'s element id derives from the
entity id.

## Data model

### Identity

Every song is keyed by a `SongId` (`u64`), derived from the
canonicalized file path via `XxHash64`. This is the reference used
everywhere — `Library`, `Album`, `StreamingInfo.current_song`, cover
cache keys — rather than passing owned `SongMetadata` copies around,
so a tag edit or cover reload only needs to happen in one place.

### `SongMetadata` — static, per-file

Loaded once via `lofty`, holds what's *declared about the file*:
title, artists, album, track position, duration, **nominal** bitrate,
lyrics (embedded), cover state.

> **Nominal vs. live bitrate:** `SongMetadata.nominal_bitrate` is the
> file's average/declared bitrate — fine for a library/browse view,
> but not what you want for a moving, real-time display, especially
> on VBR Opus/MP3. Live bitrate is computed separately, continuously,
> from decoded packet sizes during playback, and lives in
> `StreamingInfo`, not `SongMetadata`. These are different concerns
> with different lifetimes and are deliberately kept apart.

### `Lyrics` — plain vs. synced

```rust
enum Lyrics {
    None,
    Plain(String),        // from embedded ID3 USLT / Vorbis comment tags
    Synced(Vec<LyricLine>) // from a .lrc sidecar, [mm:ss.xx]text per line
}
```

Embedded tags essentially never carry timestamped lyrics — synced
lyrics come from a `.lrc` file next to the audio file, loaded lazily
(only for the currently-playing track, not the whole library) and
parsed separately from the tag-reading path.

### `StreamingInfo` — live, per-second playback state

Playback status, `current_song: Option<SongId>` (a reference, not an
owned copy), elapsed time, and `live_bitrate_bps`, updated by a
rolling window over decoded packet sizes as Symphonia produces them.

### `Library` — single source of truth

```rust
struct Library {
    songs: HashMap<SongId, SongMetadata>,
    albums: HashMap<u64, Album>, // Album.song_ids: Vec<SongId>, not owned songs
}
```

Albums, playlists, and the play queue all reference songs by
`SongId` rather than owning duplicate `SongMetadata` structs — avoids
the classic "edited the tag in one place, three other copies are now
stale" bug.

## Cover art caching

Two-tier, byte-budgeted, and deliberately **not** caching full-
resolution originals:

1. **Decode once, downscale immediately.** Embedded art can be
   multi-megabyte at 1000px+; UI never needs that. Resize down to
   whatever the largest consumer needs (e.g. ~300px for a "now
   playing" panel) before it ever touches the cache.
2. **Memory tier:** `lru::LruCache` keyed by `SongId`/album id,
   evicted by a real **byte budget**, not entry count — image sizes
   vary too much for a count-based cap to mean anything.
3. **Disk tier:** downscaled thumbnails persisted to
   `~/.cache/larkspur/covers/<id>.jpg` on first decode, so subsequent
   launches skip re-decoding embedded art for the entire library, not
   just what's currently in memory.
4. **State, not `Option`:** `CoverState::{NotRequested, Loading,
   Ready(Arc<DecodedImage>), Missing}` — distinguishes "haven't tried
   yet," "in flight," and "genuinely has no art" so the UI doesn't
   re-attempt decodes every frame or flash placeholder→real swaps.
5. **External cover fallback (planned):** `folder.jpg`/`cover.png`
   next to the audio file, common for FLAC rips without embedded art.
   `lofty` only reads embedded pictures, so this is a separate,
   album-level lookup path, not part of the per-song tag read.
6. Decoding/resizing runs off the audio and UI threads (thread pool),
   reporting back into the shared `Entity` state as each cover
   completes — library views should populate progressively, not block
   on the whole library decoding up front.

## Status

- [x] Core data schema (`model/`)
- [x] Headless playback (`PlaybackController`) + live-bitrate *tracker*
      (`bitrate.rs`, unit-tested) — meter not yet wired into playback
- [x] GPUI shell + module/workspace container (`ui/`)
- [x] Shared observable playback state (`Entity<PlaybackState>`) — views
      observe it instead of polling
- [x] Layout introspection + tests (`ui/introspect.rs`, `tests/`)
- [ ] Cover cache disk tier + external cover fallback
- [ ] `.lrc` sidecar loading wired into playback
- [ ] Live bitrate meter module (needs packet-level decode instrumentation)
- [ ] Input/keybinding system — **WIP**, not yet decided whether this
      needs to be backend-agnostic (TUI + GPUI) or GPUI-only; current
      `InputAction` enum is deliberately not tied to any specific
      input crate's key type until that's settled.
- [ ] Playlist support (same `SongId`-reference pattern as `Album`)

## Testing & layout introspection

The UI can be read as text, so layout can be reviewed and tested without
opening a window:

```sh
cargo run --bin layout_dump   # the resolved layout as an ASCII diagram
cargo test                    # model + layout tests
```

`ui/introspect.rs` resolves `ui/layout.rs`'s `LayoutPlan` into rectangles
and emits a diagram, a dock legend, and an explicit border list ("which
panel touches which"). Tests assert on the structured `LayoutReport`
rather than parsing the text. See `tests/README.md`.

## Supported formats

FLAC, MP3, Opus, OGG (Vorbis).
