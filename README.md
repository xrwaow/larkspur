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
  timestamped) lyrics, wrapped rather than clipped.
- A UI built from independent modules that can be added, removed, and
  rearranged, not a monolithic fixed layout.

## Stack

| Concern                  | Crate                          | Why                                                                 |
|---------------------------|---------------------------------|----------------------------------------------------------------------|
| Audio decode              | `symphonia`                    | Pure-Rust decoder/demuxer. FLAC and Opus are default-enabled; MP3 requires the `mp3` feature flag (licensing, not technical, reasons). AAC/MP4 (`.m4a`) comes in through rodio's `symphonia-all` feature, which cargo unifies onto this dependency. Also the packet-level source for live bitrate calculation. |
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
| `src/lib.rs` | Crate roots — `analysis`, `audio`, `bitrate`, `decode`, `model`, `opus`, `ui`. |
| `src/model/` | The whole data schema, split by concern: `identity` (`SongId` + hashing), `song` (`SongMetadata` + lofty loading; serde-derived so the scan cache round-trips it whole), `lyrics` (`Lyrics`/`LyricLine` + `parse_lrc` + `.lrc` sidecars), `cover` (`CoverState`/`DecodedImage`/`CoverCache`), `library` (`Library` + playlist autogen + derived artist groupings, with a maintained index), `playlist` (`Playlist`/`PlaylistId`/`PlaylistKind`/`PlaylistMeta`/`PlaylistOrigin`), `search` (the query language: `Query`/`parse`/`matches`, plus the album grouping both result views render), `scan` (`LibraryCache` + mtime-based incremental sync), `streaming` (`SongStatus`), `view` (`TabId`), `input` (`InputAction`), `config` (`Config`: theme, waveform style, per-container typefaces and font sizes, synced paths). Framework-agnostic — no GPUI, no rodio — so it unit-tests without a device or a window. |
| `src/audio.rs` | `PlaybackController` — owns the rodio `Player` and device sink, the `Vec<PathBuf>` queue, seek/next/prev, drain-detection auto-advance (`tick_advance`), and the current track's metadata/duration. `track_parts` is the single place decode source, tags, and duration are chosen. Deliberately does **not** own the waveform. |
| `src/decode.rs` | `open_track(path, gapless)` — the one place a Symphonia container is opened and a playable track selected. Also `PacketDecoder`, the one place a packet is turned into interleaved f32 samples: Opus goes to libopus, everything else to symphonia, and both the playback source and the analysis share it. |
| `src/opus.rs` | `OpusSource` — symphonia's Ogg demuxer feeding libopus, because symphonia 0.5 demuxes Opus but has no decoder. Handles gapless pre-skip/end trims and accurate seek (decode-and-drop to the exact timestamp). |
| `src/analysis.rs` | `analyze_track(path, buckets, bucket)` — one demux+decode pass producing both the waveform peaks and the live-bitrate profile, decoding through the shared `PacketDecoder`. `compute_waveform` is the peaks-only convenience. |
| `src/bitrate.rs` | `BitrateProfile` — a track's compressed bitrate bucketed over media time, built from packet byte counts (no decode). `at(position)` gives the live number the UI shows. |
| `src/ui/mod.rs` | UI module roots — `albums`, `animation`, `browse`, `config_state`, `container`, `cover`, `cover_store`, `format`, `input`, `introspect`, `layout`, `library_state`, `lyrics`, `menu`, `playback`, `playlist`, `playlists`, `row_list`, `search`, `settings`, `state`, `tabs`, `text_field`, `theme`, `widgets`. |
| `src/ui/main.rs` | The `larkspur_ui` binary: loads the config, seeds its synced paths from any command-line directories, and binds live views to the shared state. The layout itself lives in `src/ui/layout.rs`. |
| `src/ui/state.rs` | `PlaybackState` — the shared, observable playback `Entity`. Owns the controller, the async track analysis (waveform peaks + live-bitrate profile), and the single ticker; views observe it instead of polling. The ticker samples the player often enough to catch a track draining, but only *notifies* when a coarse value changes — the transport re-renders on the slow clock, not every sample. |
| `src/ui/library_state.rs` | `LibraryState` — the shared, observable library `Entity`. Owns the `Library`, the scan/persistence cache, and the selected playlist; roots come from `ConfigState` (the synced-path list), which it observes, so editing the list rescans. The background scan runs on the executor and the playlist views observe it. User-owned custom playlists survive a scan that was already in flight when they were edited. |
| `src/ui/playlist.rs` | `PlaylistView` — one open playlist, drawn the way the library draws an album: a cover header over its track rows, with a one-song playlist collapsed into a single row carrying its cover on the left. One instance per tab, each with its own selection. Custom playlists get a full-width “Add songs” search box that adds a clicked result, and right-clicking a track opens a menu to remove it. A custom playlist reads as “Various Artists” once its tracks disagree on the artist, and never shows a release year. `↑`/`↓` move the row selection, `enter` plays it, single click selects, double click plays. |
| `src/ui/theme.rs` | `Theme` — the color + font palette (zebra row colors, text tiers, accent, typeface). Views read it instead of hardcoding hex values. `Theme::dark()`, `Theme::light()`, and `Theme::dynamic(accent)` are the palettes; each view builds its own copy carrying its per-container typeface and font size, so a theme/font change re-renders live and one container can render larger, or in a different family, than another. |
| `src/ui/text_field.rs` | `TextField` — a minimal, view-owned text input (value + caret + selection + key handling: `ctrl+a`, `ctrl+v` paste, `shift`/`ctrl` arrows, word movement), reused by the search box, the settings path box, inline playlist rename, and the playlist's add-songs search. The caret is drawn over the text rather than between it, so moving it never nudges the characters around it. |
| `src/ui/animation.rs` | `Animator` + `Tween` + `Spring` — the shared frame clock and the easing primitives views animate with. `SLOW_FPS` (1) drives the coarse readouts (transport time, waveform progress, live bitrate); `SMOOTH_FPS` (60) drives selection/hover highlights and lyric scrolling. `Tween` is a duration-based ease (the lyric scroll); `Spring` is a damped spring that **keeps its velocity when retargeted**, so fast repeated hover chases the new target instead of restarting (gpui 0.2 has no spring element — the git version's `SpringAnimation` isn't in the published crate — so it's hand-rolled and driven by the shared clock). Views observe the clock and re-render only while something is moving. |
| `src/ui/input.rs` | `action_for_key` — the one place GPUI key names are mapped to a backend-agnostic `InputAction`. Called by the tab container and every list container, not just the transport. |
| `src/ui/row_list.rs` | `RowList` — the selection and navigation state the three list containers share: sections, flattened rows, per-section title widths, the virtualized list, the selection set, the keyboard cursor, and the selection/hover **crossfades** (per-row springs, so several rows can fade independently). `handle` maps an `InputAction` to a `RowAction`, so a view only has to say what *activating* a row means. `SPRING_STIFFNESS`/`SPRING_DAMPING` tune the highlight feel. **All** vertical movement goes through one eased `SmoothScroll` — moving the selection, paging, and the wheel (`wheel`, which the view intercepts) — so the content slides rather than stepping. |
| `src/ui/widgets.rs` | `section_header`/`empty_hint`/`panel_header` — the small view helpers the rail and the list containers had each grown their own copy of. `scroll_area`/`wheel_pixels` wrap a virtualized list so its wheel scrolling can be eased instead of applied immediately. |
| `src/ui/container.rs` | The layout system: `Container` trait, `LayoutPlan`/`Layout`/`Child`/`Size`, `Dock`/`Edge`/`Align`, `Workspace` (module registry + plan). |
| `src/ui/introspect.rs` | Resolves a `LayoutPlan` into rectangles and renders it as an ASCII diagram + dock legend + border/adjacency list. Pure geometry, no GPUI. |
| `src/ui/playlists.rs` | `PlaylistsView` — the left rail: a settings button, custom playlists, autogen album playlists, and artists (each expanding into its discography, newest release first). Clicking a playlist opens it as a tab in the center, so this view holds the tab container. Right-clicking a custom playlist opens a menu to rename it inline or delete it. A library yields thousands of rows, so like the track lists they're flattened and drawn through GPUI's virtualized `list` (rebuilt only on change, scroll kept across expand/collapse) — rendering them all every frame made the whole app run at single-digit fps. |
| `src/ui/layout.rs` | `app_layout()` — the one definition of the dock/center arrangement, shared by the real UI and the introspection tools. The center is the `tabs` container; the right rail is `lyrics` above `cover`. |
| `src/ui/albums.rs` | Album sections — the shared rendering behind the browse, search, and playlist containers: snapshot an album into a cover header plus track rows (or one compressed row when it holds a single song, carrying the release year), flatten them into the rows `gpui::list` virtualizes, measure the title column so the artists line up, and request each visible cover as it renders. Also owns `Selection`, the item-index set behind shift/ctrl click and keyboard navigation. The album meta line is tracks + time only — the bitrate that matters is per song. |
| `src/ui/tabs.rs` | `TabsView` — the center container: a strip of tabs (library, one per opened playlist, one per artist opened from “go to {artist}”) over the active container. Search is swapped in over the active tab rather than being a tab of its own. Re-points focus on every switch, so the active container is the input hub; tab switching, tab closing, the `×` buttons, and the transport keys all live here. Only the active container is in the element tree. It also sets the window title (`Playing {song}` / `Larkspur`). |
| `src/ui/browse.rs` | `BrowseView` — the library browse container. Every album as a headed group (cover, artist, album, `tracks \| time`, year) with its tracks beneath, drawn through GPUI's virtualized `list`; clicking a track plays the album from there. One instance backs the library tab; “go to {artist}” from a song menu opens another, scoped to that artist's discography (newest first), so it titles the header with the artist instead of “Library”. Selection and navigation come from `RowList`. |
| `src/ui/lyrics.rs` | `LyricsView` — the right rail's lyrics panel, above the cover. Pins the anchor line to the middle of the panel (50% of its height) whatever the height of the lines around it, and seeks to a line when it's clicked. The slide **leads the song**: the stack starts moving `TRANSITION_SECS` before a line's timestamp so it lands *on* the line, and the colour **crossfades over the second half of that slide** — the next line starts colouring up once the stack is already moving toward it, while the line it replaces fades out over the same window. Both are driven from the playback position, which is why the panel reads it at full resolution (`live_position`) on every frame of the shared clock. Lines wrap on word boundaries (and break an over-long word per character) instead of being clipped. |
| `src/ui/cover_store.rs` | `CoverStore` — the shared, observable cover store. Decodes embedded art off-thread, downscales once, writes the disk tier, and hands the UI a GPU-ready image. It also derives each cover's dominant colour at decode time (`accent`), which is what the dynamic theme tints from. One owner for both the now-playing square and the album thumbnails. |
| `src/ui/format.rs` | `format_bitrate`/`format_secs` — the shared bitrate and duration formatting the list views agree on. |
| `src/ui/cover.rs` | `CoverView` — the 240 px now-playing cover square: the current track's art, dimmed while paused. It also feeds the dynamic theme, pushing the cover store's dominant colour into `ConfigState` while that theme is selected. |
| `src/ui/playback.rs` | `PlaybackView` — the seek bar + waveform (`bars`/`line`, chosen in settings), the time labels, and the transport buttons beneath them. |
| `src/ui/settings.rs` | `SettingsView` — the settings overlay (opened from the rail's settings button): theme (dark/light/**dynamic**, derived from the current cover), waveform style, per-container typefaces and font sizes, and the synced-path list. Every change is applied and persisted immediately — there's no save. |
| `src/ui/menu.rs` | `context_menu`/`song_menu_items` — a small right-click menu helper built on GPUI's `anchored`/`deferred` (GPUI 0.2 ships no context-menu widget), and the song menu's items. A single row offers one "go to {artist}" per credited artist, "go to playlist", and "remove from playlist"; a multi-row selection offers "play", "add to {playlist}", "new playlist from selection", and "remove from playlist". The menu occludes the mouse, so clicking an item can't also play the row behind it. |
| `src/ui/config_state.rs` | `ConfigState` — the shared, observable config `Entity`. Owns the persisted `Config` and is observed by every view that draws with the theme, its typeface, or its per-container font size, so changes apply live. Also the source of truth for the synced-path list. `Themed` wraps the theme + its config subscription so a view can't forget to observe. |
| `src/bin/seek_probe.rs` | Diagnostic binary: runs the real decode + seek chain headlessly and reports seek-vs-linear RMS error. |
| `src/bin/layout_dump.rs` | Prints the resolved UI layout as text — no window needed. |
| `src/bin/library_dump.rs` | Scans directories and prints the resulting library — playlists and their songs — as text, no window. |
| `src/bin/bitrate_probe.rs` | Demuxes a file and prints its live-bitrate profile (the numbers the UI shows while playing), no window. |
| `tests/layout.rs` | Layout tests over the introspection tooling (see `tests/README.md`). |

## Running

```sh
cargo run --bin larkspur_ui                     # opens; add music in settings
cargo run --bin larkspur_ui -- ~/Music/Grimes    # also seed a synced path
cargo run --bin larkspur_ui -- song1.flac song2.opus  # queue songs for playback
cargo run --bin seek_probe  -- song.opus               # decode/seek check, no UI
cargo run --bin layout_dump                            # the UI layout as text
cargo run --bin library_dump -- ~/Music/Grimes        # what a scan produces, no UI
cargo run --bin bitrate_probe -- song.flac             # a file's live-bitrate profile
cargo test                                             # model + layout tests
```

Directories on the command line are merged into the config's synced-path list
(and persisted), so the old `larkspur_ui <dir>` invocation still works — but the
list itself lives in the config from then on, and the app no longer requires any
arguments. See [Configuration](#configuration).

## Configuration

Config lives at `$XDG_CONFIG_HOME/larkspur/config.json` (usually
`~/.config/larkspur/config.json`) and is edited through the settings overlay
(`⚙` in the rail header). Every change is applied and written immediately.

- **Theme** — `Dark`, `Light`, or `Dynamic`. The first two are fixed palettes picked by
  [`ThemeKind`](src/model/config.rs) and rebuilt per view, so switching
  re-renders live rather than needing a restart. `Dynamic` derives a palette from
  the current track's cover: the cover is analysed for its dominant colour and
  the hue drives the whole palette. It's only computed while `Dynamic` is
  selected, falls back to `Dark` until a cover has been analysed, and keeps the
  last colour when a new track's art hasn't decoded yet.
- **Waveform** — `Bars` or `Line`, how the transport bar draws the waveform.
- **Typeface, per container** — every container draws with
  [`FontKind::default`](src/model/config.rs) (`TX-02`) unless config overrides
  it, keyed by `container_id` like the font size. The settings row's family
  button opens a menu of the available families (each drawn in its own face); a
  family the system doesn't have falls back to the text system's default.
- **Font size, per container** — every container inherits a default from
  `Container::default_font_size` and can be overridden in config, keyed by
  `container_id`. A view builds its `Theme` at its own size, and the column
  widths derive from it (see `Theme::cell_px`/`num_col`/…), so raising one
  container's size scales its columns with its text. “Reset” clears both the
  typeface and the size override.
- **Synced paths** — the directories scanned into the library. This list, not
  the command line, is the source of truth. Adding a path triggers a rescan;
  removing one drops the cached songs and stamps that no *remaining* root still
  covers, then rescans — so removing `b` leaves `a/b`'s contents alone if `a/b`
  is still synced, but removing `a` takes `a/b` with it. Nested roots are
  deduped, since the parent's walk already covers the child.

## Keys

Every binding is mapped to a backend-agnostic [`InputAction`](src/model/input.rs)
in one function, `ui::input::action_for_key` — the only place GPUI key names
appear. The active container gets first crack at a key; anything it doesn't act
on bubbles up to the tab container.

| Key | Action |
|-----|--------|
| `space` | play / pause |
| `←` / `→` | seek back / forward 5 s |
| `↑` / `↓` | move the row selection |
| `pgup` / `pgdn` | scroll the active list a page |
| `enter` | play the selected row (or, in search, run a changed query) |
| `ctrl+tab` / `ctrl+shift+tab` | next / previous tab |
| `ctrl+w` | close the active tab (or dismiss an overlay) |
| `ctrl+shift+f` | open / close the search panel |
| `esc` | dismiss the search or settings panel, or a right-click menu |

Mouse actions the panels own: click a playlist in the rail to open it, click a
track to play the album/playlist from it (a playlist tab selects on a single
click and plays on a double), `shift`+click to extend a range and `ctrl`+click
to toggle one, right-click a **custom** playlist to rename or delete it, and
right-click a track for **go to {artist}** (one entry per credited artist,
opening the library scoped to that artist's discography) / **go to playlist**
(its album), plus **remove from playlist** when it's in a custom one. A
right-click on a multi-row selection offers the bulk operations instead: play,
add to a playlist, create a playlist from it, and remove from a custom one. The
rail's `⚙` button opens settings.

The text boxes (search, settings path, playlist rename, add-songs) support the
usual editing chords: `ctrl+a` selects all, `shift`+arrows extend the selection,
`ctrl`+arrows move by word, and `ctrl+v` pastes.

While the search panel is up it owns plain typing and editing keys, so a space
is a space rather than a play/pause. Command chords and `esc` still bubble, so
tab switching and closing keep working from inside the box. The settings panel's
path box behaves the same way, and a playlist's add-songs box while it's
focused.

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
                 │  Entity<PlaybackState>│      for static + live state
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
> `LibraryState` (also an `Entity`) owns the `Library`, the scan, and the
> active playlist, and backs every library-reading view; `CoverStore`
> (a third `Entity`) owns decoded cover art for all of them, so the
> now-playing square and the album thumbnails share one decode cache.
> `Animator` (a fourth) is the shared frame clock. The bitrate-meter and
> queue modules don't exist.

### Frame rates

Two clocks, and views opt into the one they need:

- **`SLOW_FPS` (1)** — the transport's coarse readouts: time labels, waveform
  progress, live bitrate. `PlaybackState` samples the player often enough to
  catch a track draining, but only *notifies* when a coarse value changes, so a
  playing track re-renders the UI about once a second rather than 60 times.
- **`SMOOTH_FPS` (60)** — animation. `Animator` is the shared clock; views that
  animate (selection and hover highlight springs, lyric scrolling) observe it
  and re-render only while something is actually moving, so an idle UI costs
  nothing beyond the clock's wake-ups.

Both are plain constants in `ui/animation.rs`. Dev builds also turn
`debug-assertions` off (see `Cargo.toml`) — GPUI compiles its element inspector
and a pile of per-frame checks behind that flag, which is a large chunk of frame
time.

### UI layout (current)

`ui/layout.rs` defines the workspace from edge docks plus a center,
resolved by `ui/container.rs`; `ui/main.rs` only binds live views to its
module ids. Run `cargo run --bin layout_dump` to see it as a diagram:

| Dock | Module(s) | Band size | `z` |
|------|-----------|-----------|-----|
| bottom | `playback` (transport bar) | content height | 1 |
| right | `lyrics` (fills) + `cover` | 240 px | 2 |
| left | `playlists` | 240 px | 2 |
| center | `tabs` | — | — |

`Workspace::render_dock_stack` applies the highest `z` first: it hugs
the window edge, and lower-`z` docks fit into what remains. Side docks
(`left`/`right`) carry a −1 handicap against top/bottom docks, which is
exactly why the two 240 px rails need `z = 2` — it lets them span the
full window height and puts the bottom bar *between* them, keeping the
transport bar centered in the window instead of shifted left by the
cover. Without the `z`, the bottom bar wins the full width and the
rails sit above it instead of beside it.

The center is a single `tabs` module rather than one module per view. Tabs
are a *runtime* concern: `LayoutPlan` stays plain data (no `Entity`s, no
`App`) so `ui::introspect` and the layout tests can resolve it without a
window, which is exactly what would break if tab instances lived in the
layout. `TabsView` owns the containers instead, and only the active one is
mounted — so only it receives keys. The search and settings panels ride the
same mechanism: neither is a tab, `ctrl+shift+f` swaps search in over the
active one and the rail's settings button swaps settings in.

## Data model

### Identity

Every song is keyed by a `SongId` (`u64`), derived from the
canonicalized file path via `XxHash64`. This is the reference used
everywhere — `Library`, `Album`, `PlaybackState.current_song`, cover
cache keys — rather than passing owned `SongMetadata` copies around,
so a tag edit or cover reload only needs to happen in one place.

### `SongMetadata` — static, per-file

Loaded once via `lofty`, holds what's *declared about the file*:
title, artists, album, track position, **year**, duration, **nominal**
bitrate, lyrics, cover state.

> **Year** comes from the first of `Year` / `RecordingDate` / `ReleaseDate`
> that parses — the three keys taggers scatter it across. It drives the
> browse header's year column and the `year:` search filter, and a song with
> none simply never matches a date filter.

> **Nominal vs. live bitrate:** `SongMetadata.nominal_bitrate` is the
> file's average/declared bitrate, normalized to **bits per second** (lofty
> reports kbps) — fine for a library/browse view, but not what you want for a
> moving, real-time display, especially on VBR Opus/MP3. The *live* bitrate
> comes from `BitrateProfile` (the track's per-bucket bitrate curve, built in
> the same pass as the waveform), indexed by playback position and surfaced
> through `PlaybackState`. These are different concerns with different
> lifetimes and are deliberately kept apart.

### `Lyrics` — plain vs. synced

```rust
enum Lyrics {
    None,
    Plain(String),        // from embedded ID3 USLT / Vorbis comment tags
    Synced(Vec<LyricLine>) // from a .lrc sidecar, [mm:ss.xx]text per line
}
```

Embedded tags essentially never carry timestamped lyrics, so synced lyrics
come from a `.lrc` file next to the audio file. A sidecar wins over an
embedded tag when both exist, since it's the only source that reliably has
timestamps; some taggers *do* embed LRC-formatted text, which is why the
tag contents are sniffed rather than trusted.

Lyrics are read **at scan time** and cached with the rest of the metadata,
which is what makes lyrics searchable across the whole library. A song's
cache stamp is the newer of the audio file's mtime and its sidecar's, so
editing a `.lrc` re-parses that one song instead of leaving stale text.

### `Config` — persisted preferences

The runtime configuration, kept framework-agnostic in `model/config.rs` and
wrapped as an observable `Entity` by `ui::config_state::ConfigState`: the theme
kind, the waveform style, per-container typeface and font-size overrides, and
the synced-path list. It's the source of truth for what gets scanned (see
[Configuration](#configuration)), and views observe it so theme/font changes
apply live. The dynamic theme's accent is runtime state on `ConfigState` (not
persisted): `CoverView` pushes it in, and `theme_for` reads it to build the
palette.

### `Library` — single source of truth

```rust
struct Library {
    songs: HashMap<SongId, SongMetadata>,
    playlists: HashMap<PlaylistId, Playlist>, // Playlist.song_ids: Vec<SongId>
    next_playlist_id: u64,
    index: Index,   // derived, rebuilt on mutation
}
```

Songs, playlists, and the play queue all reference songs by `SongId`
rather than owning duplicate `SongMetadata` structs — avoids the classic
"edited the tag in one place, three other copies are now stale" bug.

The fields are **private** and the derived `Index` (which album owns each song,
which playlists each artist appears on, each playlist's release year) is
rebuilt by every mutating method — so the hot queries (`album_playlist_of`,
`discography`, `playlist_year`, the search grouping) are lookups rather than
scans, and can't be bypassed into going stale. Bulk paths go through
`replace_songs`/`replace_playlists`/`install`, so the cache never reaches into
the library's guts.

### Playlists — one container, one collection

There is no separate `Playlists` type: `Library` **is** the collection (the
"list of playlists"), and `Playlist` is the container. Autogen and custom
playlists share one schema (`PlaylistMeta`); they differ only in where the
metadata comes from and whether a scan rebuilds them:

- **`PlaylistKind::Auto`** — one per album tag (keyed by album title + album
  artist, so a compilation stays one playlist), rebuilt by `Library::rebuild_auto`
  on every scan and read-only. An id is preserved across rebuilds by matching
  `PlaylistOrigin`, so a rescan doesn't churn ids.
- **`PlaylistKind::Custom`** — user-owned: `add_song`/`remove_song`/`rename`/
  `remove_playlist`. Ids come from a persisted monotonic counter, so two
  playlists may share a title.

An *artist* is not a playlist — it's a list of playlists (their albums), so
`Library::discography(artist)` and `Library::artist_appears_in(artist)` are
**derived** on demand rather than stored. `PlaylistMeta.cover` is `None` for
"inherit": an autogen playlist shows its first song's cover, and so does a
custom one (until custom covers land).

### Scanning — mtime-based incremental sync

The library is built from a list of directories (`LibraryCache::sync`). Each
root and each directory under it carries an mtime stamp; an unchanged tree is
skipped entirely, and within a changed tree only files whose mtime moved are
re-parsed. A song's stamp is the newer of the audio file's mtime and its
`.lrc` sidecar's, so editing lyrics re-reads that song. Metadata — including
year and lyrics — is cached to `~/.cache/larkspur/library.json` (versioned,
written via a temp file + rename); audio is never cached — it's decoded at
play time — and cover pixels live in the cover cache instead.

### Search — one engine, three views

`model/search.rs` owns all matching: `parse` turns text into a `Query`,
`Query::matches` decides whether one song satisfies it, and `search` applies
that over the `Library`. It's pure and framework-agnostic, so the query
language is unit-tested without a window, and no view implements matching
itself.

The search view isn't a tab: `ctrl+shift+f` swaps it in over whatever tab is
active and `esc` puts it back, so searching is a detour rather than another
thing to manage.

The three center containers are the same rendering fed from three entry points:

- `all_albums(library)` — the browse view. No query at all, just the library
  grouped by album.
- `search(library, &query)` — the search view. Filtered, then grouped by the
  same rule.
- `section_for_playlist(library, id)` — an opened playlist. One album section,
  which is what makes a playlist tab look like the library.

They all hand back album sections, which `ui::albums` draws (cover, artist,
album, format line, year, tracks) — including collapsing a single-song section
into one row with its cover on the left.

#### Query language

Terms are whitespace-separated; `"..."` groups a phrase. A term whose prefix
is a known keyword is a directive, anything else is free text. **Free words
are ANDed** — every word has to appear, in any order — so `nancy sinatra`
finds both "Nancy Sinatra" and "Sinatra, Nancy".

```text
type:MODE            exact | contains | fuzzy        (default: contains)
from:FIELD[,FIELD]   artist | album | song | lyrics  (default: all)
artist:TEXT  album:TEXT  title:TEXT  lyrics:TEXT     (per-term field scope)
year:SPEC            YYYY | YYYY-YYYY | >=YYYY | <=YYYY | >YYYY | <YYYY
dur:SPEC             M:SS | M:SS-M:SS | >M:SS | <M:SS   (alias: length:)
ext:LIST             flac,mp3,…                         (alias: file:)
```

```text
sinatra                          # any field, substring
type:fuzzy gnga                  # subsequence: "gnga" -> "GINGA"
from:artist type:exact sinatra   # the word "sinatra" in an artist name
title:"bang bang" year:2003
dur:>5:00 ext:flac
```

Match modes, precisely: `contains` is a substring of the field; `exact` is the
whole field or one whitespace-delimited word of it; `fuzzy` is a subsequence
(the term's characters in order, gaps allowed). `>`/`<` are exclusive for both
years and durations, and a song with no year never matches a date filter.

An unrecognised keyword-shaped prefix (`yeer:2003`) is reported as a warning
and kept as free text, so a typo degrades instead of erroring — and a bare
`3:00` stays free text rather than being read as an unknown keyword. Only the
fields a query actually reads are lowercased, so a query that never touches
lyrics never joins them.

## Cover art caching

Two-tier, byte-budgeted, and deliberately **not** caching full-
resolution originals. `model::CoverCache<T>` owns the budgeting (it's generic
over the payload, so the framework-agnostic model owns the policy while the
UI stores what it renders from); `ui::cover_store::CoverStore` is the shared
`Entity` both cover consumers read. Decoded pixels are RGBA and GPUI's
`RenderImage` is BGRA, so the two channels are swapped on the way in — get that
wrong and nothing breaks, every cover just renders with red and blue
exchanged.

1. **Decode once, downscale immediately.** Embedded art can be
   multi-megabyte at 1000px+; nothing in the UI needs that. Decode, resize
   to 300px, and only then cache.
2. **Memory tier:** `lru::LruCache` keyed by `SongId`, evicted by a real
   **byte budget** (64 MiB), not entry count. `insert` returns the ids it
   evicted, and `CoverStore` drops the same entries from its GPU-image map —
   so there's one budget, not two that drift.
3. **Disk tier:** downscaled thumbnails persisted to
   `~/.cache/larkspur/covers/<id>.jpg` on first decode, so subsequent
   launches skip re-decoding embedded art for the entire library, not just
   what's currently in memory. A disk hit short-circuits the file entirely.
4. **State, not `Option`:** `CoverImage::{Loading, Ready(Arc<RenderImage>),
   Missing}` — distinguishes "in flight" from "genuinely has no art", so the
   UI doesn't re-attempt decodes every frame or flash a placeholder→real swap.
   A song whose tags declare no picture is recorded `Missing` without ever
   opening the file.
5. **External cover fallback (planned):** `folder.jpg`/`cover.png` next to
   the audio file, common for FLAC rips without embedded art. `lofty` only
   reads embedded pictures, so this is a separate, album-level lookup path,
   not part of the per-song tag read.
6. **Off the UI thread, and batched.** Decoding runs on the background
   executor and reports back into the shared `Entity`, so covers populate
   progressively. Views ask the store for the first 24 unknown covers per
   frame; each decode landing re-renders and requests the next batch, so a
   large library fills in over a few frames instead of spawning hundreds of
   decodes at once.

## Status

- [x] Core data schema (`model/`)
- [x] Headless playback (`PlaybackController`) + live-bitrate *tracker*
      (`bitrate.rs`, unit-tested) — meter not yet wired into playback
- [x] GPUI shell + module/workspace container (`ui/`)
- [x] Shared observable playback state (`Entity<PlaybackState>`) — views
      observe it instead of polling
- [x] Layout introspection + tests (`ui/introspect.rs`, `tests/`)
- [x] Cover art — `CoverStore` decodes embedded art off-thread, downscales
      once, writes the `~/.cache/larkspur/covers/` disk tier, and feeds both
      the now-playing square and the album thumbnails
- [ ] External cover fallback (`folder.jpg`/`cover.png`)
- [x] Lyrics — embedded and `.lrc` sidecar lyrics read at scan time and cached
      with the metadata, plus the right-rail lyrics panel. Synced lines keep the
      active one pinned to the middle of the panel and seek when clicked
- [x] Lyrics wrapping — long lines wrap on word boundaries and break an
      over-long word per character instead of being clipped, and each line keeps
      its height (`flex_none`) so a wrapped line can't paint over the next. The
      active line is shown by text colour alone, not a background band. The
      middle-of-panel centring is approximate once a line wraps, since the
      anchor is the middle *slot*, not necessarily the middle pixel
- [x] Center tabs — one container per tab (`TabsView`): library, plus one per
      opened playlist. Opened from the left rail, `ctrl+tab` cycles, `ctrl+w`
      or `×` closes
- [x] Browse container — every album as a headed group (cover, artist, album,
      format line, year) with its tracks beneath
- [x] Playlist tabs — an opened playlist is drawn with the same album rendering
      as the library, one section per playlist (single-song playlists collapse
      to one row with the cover on the left). No play button — clicking a track
      plays it. Custom playlists add songs through the search engine (a
      half-width search box under the current songs) and remove a track from a
      right-click menu; renaming and deleting the playlist live in the rail's
      right-click menu
- [x] Config + settings — a persisted `Config` (theme, per-container typefaces
      and font sizes, synced paths) with a settings overlay opened from the rail.
      Theme and font changes apply live; the synced-path list replaces command-
      line directories as the source of truth, and removing a path updates the
      cache without dropping anything a remaining root still covers
- [x] Search — `model/search.rs`'s query language (exact/contains/fuzzy, field
      scope, year/duration/extension filters) and the results container, opened
      with `ctrl+shift+f` as an overlay rather than a tab
- [ ] Views — saved collections of tabs: which tabs are visible/active and
      where they sit (split / focus mode), persisted via `model::Config`
- [x] List virtualization — the browse, search, and playlist containers draw
      through `gpui::list`, so only the rows near the viewport (plus a modest
      overdraw band) are rendered and their covers requested. A 4k-song library no
      longer renders every row each frame or thrashes the cover store
- [ ] Search ranking — fuzzy matches aren't scored, so results come out in
      album/track order rather than by match quality
- [x] Live bitrate — `BitrateProfile` (per-track curve, computed in the same
      pass as the waveform), shown on the playing track's row in the list views
      (next to its duration) and refreshed every 250 ms of media time. Every
      row shows its declared bitrate; the playing one switches to the live
      number
- [ ] Input/keybinding system — **WIP**, not yet decided whether this
      needs to be backend-agnostic (TUI + GPUI) or GPUI-only; current
      `InputAction` enum is deliberately not tied to any specific
      input crate's key type until that's settled.
- [x] Playlist support — `Playlist` container + `Library` collection,
      autogen album playlists and derived artist discographies, custom
      playlists (add/remove/rename/delete), and mtime-based directory scan
      with a JSON metadata cache. User-owned custom playlists now survive a
      scan that was in flight while they were edited (the startup scan used to
      silently undo a playlist created right after launch)
- [x] Multi-select — `shift`+click extends a range, `ctrl`+click toggles a row,
      `↑`/`↓` move a keyboard cursor and `enter` plays it, and `pgup`/`pgdn`
      scroll a page. A right-click on a multi-row selection offers play / add to
      a playlist / new playlist / remove from playlist
- [x] Dynamic theme — a palette derived from the current cover's dominant
      colour, selectable in settings and only computed while selected
- [x] Waveform style in settings, transport controls below the waveform, the
      titlebar naming the current track, clipboard paste in every text box, and
      discographies ordered newest-first (undated albums last)
- [x] Shared structure — `Library` keeps a derived index (song→album,
      artist→playlists, playlist→year) behind private fields, so the hot queries
      are lookups and can't go stale; the scan cache stores a whole
      `SongMetadata` instead of mirroring its fields; one `PacketDecoder` serves
      playback and analysis; and the three list containers share `RowList`,
      `Themed`, `widgets`, and one key mapping
- [x] Frame rates — a shared `Animator` clock with swappable `SLOW_FPS` (1, for
      the transport readouts) and `SMOOTH_FPS` (60, for selection fades and lyric
      scrolling), and a dev profile with GPUI's debug-only inspector off

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

FLAC, MP3, Opus, OGG (Vorbis), M4A/M4B (AAC/ALAC).
