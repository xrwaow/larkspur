# Larkspur — data-shape & dataflow optimization TODO

> Generated from a full read of `src/`. Work top-down: Phase A+B are mechanical
> and unblock the rest. Each task lists the current shape, the target shape, and
> what to verify. Line numbers are approximate; grep the named symbol.
>
> Verify after each phase:
> `cargo test --all-targets`
> `cargo clippy --all-targets -- -D warnings` (or at least inspect new warnings)
> Build a dev run and exercise: launch, scan, browse, search, playlist open,
> drag-to-reorder queue, play/pause/next/prev, theme change, font change.

---

## Phase A — Identity, queue, and enum shapes

### A1. Make `SongId` a real newtype
- **Current:** `src/model/identity.rs`: `pub type SongId = u64;`
- **Problem:** any `u64` (playlist id, generation, count, hash) is accepted where
  a song id belongs. `PlaylistId` is already a newtype, so this is an
  inconsistency.
- **Change:**
  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
  #[serde(transparent)]
  pub struct SongId(pub u64);
  ```
  - `generate_song_id` returns `SongId(hasher.finish())`.
  - Add `impl SongId { pub fn get(self) -> u64 { self.0 } }` only if needed.
  - Update every construction site: tests' `song(id, ...)` helpers, `SongId::from` if
    useful, `HashMap<SongId, _>` keys, serde round-trips.
- **Watch out:** `Stats(HashMap<SongId, SongStats>)` and playlist `song_ids` are
  persisted as JSON. `#[serde(transparent)]` should keep the same wire shape;
  add a round-trip test in `src/model/stats.rs` and `src/model/scan.rs`.
- **Verify:** compiler catches all old `u64` call sites; `cargo test`.

### A2. Introduce `QueueEntry` in the model
- **Current:** `(SongId, PathBuf)` tuples in:
  - `src/audio.rs`: `PlaybackController.queue`, `set_queue`, `add_to_queue`,
    `queue()`, `remove_from_queue`, `move_entry`, `current_path`.
  - `src/ui/playback_state.rs`: `add_to_queue`, `play_paths`, `queue`.
  - `src/ui/library_state.rs`: `play`, `ensure_songs`, `play_files`.
  - `src/ui/menu.rs`: add-to-queue and bulk add-to-queue.
  - `src/ui/queue.rs`: `queue_section`.
- **Change:** new `src/model/queue.rs`:
  ```rust
  pub struct QueueEntry { pub song: SongId, pub path: PathBuf }
  ```
  Export from `src/model/mod.rs`. Use it everywhere above.
- **Placement matters:** `audio.rs` is below `ui`, so the type must live in
  `model`, not `ui`.

### A3. Add `LibraryState::queue_entries`
- **Current:** three separate “id → metadata/path” resolutions:
  - `LibraryState::play` (`src/ui/library_state.rs`)
  - `menu.rs` add-to-queue closure
  - `QueueView::queue_section` (`src/ui/queue.rs`)
- **Change:**
  ```rust
  pub fn queue_entries(&self, songs: &[SongId]) -> Vec<QueueEntry> {
      songs.iter()
          .filter_map(|id| self.library.get(*id).map(|s| QueueEntry { song: s.id, path: s.path.clone() }))
          .collect()
  }
  ```
  Use it in `play`, `play_files`/`ensure_songs`, `menu.rs`, and anywhere else
  resolving ids to paths for the controller.
- **Acceptance:** grep for `(SongId, PathBuf)` returns only `QueueEntry` internals.

### A4. Remove the `PlaylistId(0)` queue sentinel
- **Current:** `src/ui/queue.rs:143,179` builds an `AlbumSection` with
  `playlist: PlaylistId(0)` and special-cases row actions.
- **Change:** in `src/ui/albums.rs`:
  ```rust
  pub enum SectionSource { Playlist(PlaylistId), Queue }
  pub struct AlbumSection { pub source: SectionSource, ... }
  ```
  Update `section`, `section_for_playlist`, `sections_from_groups`,
  `row_target`, `row_song`, `render_item`, `RowActions`, and queue row actions.
  Alternatively `playlist: Option<PlaylistId>` if the enum is too invasive.
- **Acceptance:** no fake ids; next real playlist id cannot alias the queue.

### A5. Replace `SongMenuRequest`’s two booleans with an origin enum
- **Current:** `src/ui/menu.rs`: `SongMenuRequest { songs, position, playlist, queue, card }`.
  `queue: bool` and `card: Option<PlaylistId>` encode mutually exclusive origins.
- **Change:**
  ```rust
  pub enum MenuOrigin {
      Row { playlist: Option<PlaylistId> },
      Queue,
      Card(PlaylistId),
  }
  pub struct SongMenuRequest { pub songs: Vec<SongId>, pub position: Point<Pixels>, pub origin: MenuOrigin }
  ```
  Update `song_menu_items`, `raise_menu`, `CoverView` right-click, `QueueView`.
- **Acceptance:** no invalid combinations representable.

---

## Phase B — Library mutation & index performance

### B1. Batch mutations / single reindex
- **Current:** `Library::insert_song` calls `reindex()` on every insert
  (`src/model/library.rs`); `LibraryState::ensure_songs` inserts files one by one.
  A scan reindexes in `replace_songs`, `prune`, `rebuild_auto`, then
  `merge_scanned` reindexes again via `adopt_custom_from`/`prune`.
- **Change:**
  - Add `Library::insert_songs(Vec<SongMetadata>)` that inserts all, then reindexes once.
  - Add a batch guard:
    ```rust
    pub struct LibraryBatch<'a> { lib: &'a mut Library }
    impl Library { pub fn batch(&mut self) -> LibraryBatch<'_>; }
    impl Drop for LibraryBatch<'_> { fn drop(&mut self) { self.lib.reindex(); } }
    ```
    or a `reindex_dirty: bool` + `pub fn begin_batch/end_batch`.
  - Suppress nested `reindex()` inside the batch.
  - Use the batch in `scan::sync` (replace/prune/rebuild_auto) and in
    `LibraryState::merge_scanned` (adopt/prune).
- **Acceptance:** one `reindex` per scan and per bulk insert; add a test counting
  reindexes (a `#[cfg(test)] reindex_count` counter).

### B2. Cache derived library views in `Index`
- **Current:** `Library::playlists()` allocates, filters and sorts on every call
  (`src/model/library.rs`); `custom_playlists()` sorts every call;
  `smart_playlists()` linear-scans per kind; `artists()` clones every string.
- **Change:** extend `struct Index`:
  ```rust
  sorted_playlists: Vec<PlaylistId>,   // auto by title, then custom
  custom_playlists: Vec<PlaylistId>,   // creation order
  smart_by_kind: [Option<PlaylistId>; SmartKind::ALL.len()],
  // existing: artists, song_album, artist_playlists, playlist_release
  ```
  Rebuild them in `reindex()`. Have `playlists()`, `custom_playlists()`,
  `smart_playlists()` iterate cached ids and resolve via `self.playlists`.
- **Note:** keep the same public return types (`Vec<&Playlist>`) initially to
  limit churn; switch to iterators later.

### B3. Stop allocating in comparators
- **Current:**
  - `SongMetadata::display_title` returns `String` (`src/model/song.rs`).
  - `Library::title_of` returns `String` (`src/model/library.rs`) and is called
    twice per comparison inside `sort_by_year`.
  - `SmartKind::title_id` (`src/model/playlist.rs`) calls `display_title()` inside
    `sort_by` comparisons.
- **Change:**
  - `pub fn display_title(&self) -> Cow<'_, str>`; add `.into_owned()` only where
    ownership is required.
  - `pub(crate) fn title_of(&self, id: PlaylistId) -> &str`:
    `self.playlists.get(&id).map_or("", Playlist::title)`.
  - In `SmartKind::select`, decorate each entry with its sort key once:
    `(song, stats, display_title)` then `sort_by` on precomputed values.
  - In `Library::rebuild_auto`, use `sort_by_cached_key` or decorate with
    `(track_position, title, id)` so `HashMap` lookups and string clones happen
    once per item.
- **Acceptance:** `cargo clippy` clean; no `String` allocation in sort closures.

### B4. Hash-set membership for bulk operations
- **Current:**
  - `Library::add_songs` calls `p.contains(song)` per song.
  - `Library::remove_songs` does `retain(|s| !songs.contains(s))`.
  - `PlaybackController::remove_from_queue` does `retain(|(id,_)| !ids.contains(id))`.
- **Change:** build `HashSet<SongId>` (or `HashSet<&SongId>`) for inputs above a
  small threshold; keep linear scan for tiny slices if desired.
- **Acceptance:** bulk add/remove of thousands of songs is not quadratic.

### B5. Incremental index updates (after B1/B2)
- `add_songs`, `remove_songs`, `create_custom`, `remove_playlist` currently
  reindex everything. Once `Index` is cached, update only the affected
  playlist’s `artist_playlists`/`playlist_release`/sorted vectors.
- Keep `reindex()` as the fallback, call it only for bulk/song-set changes.

### B6. `SongStats: Copy`
- `src/model/stats.rs`: add `Copy`; `Stats::get` no longer needs `.cloned()`.
- This also speeds `SmartKind::select`, which does `stats.get(song.id)` per song.

### B7. (Optional) Move `Stats` into the library layer
- **Current:** `LibraryState` owns `stats` and calls `library.rebuild_smart(&stats)`
  after every play; smart playlists store denormalized `song_ids`.
- **Change:** either move `Stats` into `Library` (persisted separately, as now) or
  introduce a `LibrarySnapshot { songs, playlists, stats }`. Then smart playlists
  become normal queries and `LibraryState` loses parallel bookkeeping.
- **Risk:** touches persistence and every `rebuild_smart` call; do last in Phase B.

---

## Phase C — Song metadata, presentation, and search

### C1. Precompute presentation labels
- **Current:** every `albums::section` rebuild computes
  `display_title()`, `artists.join(", ")`, `format_duration`, `format_bitrate`
  for every row (`src/ui/albums.rs`). The same work repeats across browse,
  search, playlist, queue tabs and on every library revision.
- **Change:** introduce
  ```rust
  pub struct SongDisplay {
      pub title: SharedString,
      pub artist: SharedString,
      pub duration: SharedString,
      pub bitrate: SharedString,
  }
  ```
  Cache it keyed by `SongId` and invalidated by `LibraryState::revision`.
  Possible homes: a small `DisplayCache` in `LibraryState`, or a per-view cache.
  `TrackRow` then borrows/clones `SharedString`s (cheap Arc bumps).
- **Acceptance:** opening several tabs does not re-run `join`/format for the same
  songs; measure with a temporary counter if needed.

### C2. Cache normalized search text
- **Current:** `search::Haystack::new` lowercases artist/album/title/lyrics for
  every song on every query (`src/model/search.rs`). Browse’s filter rebuilds
  this on every keystroke.
- **Change:** add a lazily built `SearchIndex` keyed by `SongId` with lowercased
  fields, invalidated by library revision. Build it on first query in
  `LibraryState` (or `Library`) and reuse.
- **Acceptance:** repeated queries over a large library don’t re-lowercase song
  metadata; add a benchmark-style test or counter.

### C3. `FieldSet` instead of `Vec<Field>`
- **Current:** `Query.fields: Vec<Field>`, `Query::needed_fields` returns
  `Vec<Field>`, membership uses `contains`.
- **Change:** `#[derive(Clone, Copy)] pub struct FieldSet(u8)` with
  `contains`, `insert`, `union`, `empty`, `ALL`. `Query.fields: FieldSet`,
  `Matcher.needed: FieldSet`. Update `from:` parsing and `Haystack::new`.
- **Acceptance:** no allocation for field scope; tests unchanged.

### C4. Unify the rail’s text filter with model search
- **Current:** `src/ui/playlists.rs` `GroupFilter::query` + `query_matches` does
  lowercase substring matching, separate from `model::search`.
- **Change:** expose a small reusable primitive in `model::search`, e.g.
  ```rust
  pub struct TextMatcher { needle: String, mode: MatchMode }
  impl TextMatcher { pub fn new(input: &str, mode: MatchMode) -> Self; pub fn matches(&self, hay: &str) -> bool; }
  ```
  Use it for rail album/artist filters; optionally accept full `Query` syntax
  later. Delete `query_matches`.
- **Acceptance:** one matching implementation; behavior documented.

### C5. Rename `AlbumGroup` → `PlaylistGroup` / `SongGroup`
- `src/model/search.rs` `AlbumGroup` is used for custom/smart/temporary
  playlists too. Rename across `search.rs`, `select.rs`, `albums.rs`, and tests.

### C6. Collapse search helper wrappers
- `all_albums`, `search`, `matching_songs` all wrap `Library::select` with
  different `Selection`s (`src/model/search.rs`). Introduce one
  `Library::grouped(scope, query, order)` and have callers use it directly, or
  keep exactly one convenience wrapper. `all_albums` is public but tests-only.
- **Acceptance:** no dead public API; no duplicate selection setup.

### C7. (Optional) Intern artist/album strings
- `SongMetadata` owns `String`s; `Index.artists` clones them into a `BTreeSet`,
  and `artist_playlists` clones them again as map keys.
- Consider `Arc<str>` for artist/album fields (serde-friendly with `rc` feature)
  or a small interner keyed by `&str`.
- **Risk:** serde schema and all call sites; measure before doing.

---

## Phase D — Persistence schema

### D1. Per-file schema versions
- **Current:** one `CACHE_VERSION` in `src/model/scan.rs` is used for
  `library.json`, `playlists.json`, and `stats.json`. A song-schema bump drops
  user playlists and stats.
- **Change:** `CACHE_VERSION`, `PLAYLISTS_VERSION`, `STATS_VERSION`
  (or one envelope with per-section versions). Bump only what changed.
- **Acceptance:** changing song fields does not invalidate playlists/stats; add
  a version-mismatch test per file.

### D2. Centralize atomic JSON persistence
- **Current:** duplicated temp-file+rename writers:
  - `src/model/scan.rs::write_json`
  - `Config::save` in `src/model/config.rs`
- **Change:** new `src/model/persist.rs` (or `src/persist.rs`):
  ```rust
  pub fn read_json<T: DeserializeOwned>(path: &Path) -> Option<T>;
  pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T, pretty: bool) -> anyhow::Result<()>;
  ```
  Use compact JSON for `library.json`/`stats.json`; keep pretty for `config.json`.
- **Acceptance:** one implementation; crash-safety test remains.

### D3. Deterministic serialization order
- **Current:** `CacheFile.dirs`/`songs` are `HashMap`s; `Config.fonts`/`font_sizes`
  are `HashMap`s. Save order is nondeterministic.
- **Change:** `BTreeMap` or sort vectors before writing (`songs.sort_by_key(|s| s.song.id)`,
  `dirs` sorted). Apply to `Config` maps too.
- **Acceptance:** saving twice without changes produces identical bytes.

### D4. Key the song cache by `SongId`
- **Current:** `LibraryCache.entries: HashMap<PathBuf, CachedSong>` duplicates
  `SongMetadata.path`.
- **Change:** key by `SongId`; use `entry.song.path` for staleness checks. Keep a
  temporary `path → id` map during one walk if needed.
- **Acceptance:** cache round-trip and incremental scan tests pass.

### D5. Config: one `ContainerStyle` map + `ContainerId`
- **Current:** parallel `fonts: HashMap<String, FontKind>` and
  `font_sizes: HashMap<String, f32>` in `src/model/config.rs`.
- **Change:**
  ```rust
  pub struct ContainerStyle { pub font: Option<FontKind>, pub size: Option<f32> }
  pub styles: BTreeMap<ContainerId, ContainerStyle>, // serde as string keys
  ```
  `ContainerId` is an enum with `as_str`/`FromStr`, replacing stringly-typed keys
  shared between `Config`, `ConfigState`, and `ui/settings.rs`.
- **Migration:** `#[serde(default)]` + `#[serde(alias)]` or a one-time migration
  from the old two maps; bump config version.
- **Acceptance:** settings round-trip; old config still loads.

### D6. (Optional) `SongStats` timestamps as epoch millis
- Replace `SystemTime` in `src/model/stats.rs` with `u64` epoch millis (or a
  small `Timestamp` newtype) for a simpler, smaller `stats.json`. Keep conversion
  at the edges.

---

## Phase E — Scan pipeline

### E1. Single filesystem walk
- **Current:** `scan.rs` calls `collect_dirs(root)` then `collect_files(root)` —
  two full traversals per changed root.
- **Change:** one walk returning `(HashMap<PathBuf, SystemTime>, Vec<PathBuf>)`.
  Collect audio-file and `.lrc` mtimes in the same pass.
- **Acceptance:** scan tests unchanged; count `read_dir` calls in a test if useful.

### E2. Avoid per-file canonicalization
- **Current:** `SongMetadata::load` → `generate_song_id` canonicalizes every file.
- **Change:** roots are already canonicalized by `normalize_roots`; derive child
  ids from canonical `root.join(...)` paths, falling back to `canonicalize` only
  when the path could contain symlinked components.
- **Acceptance:** ids remain stable for the same file; symlink test still passes.

### E3. Refresh the “fast path” documentation / behavior
- The unchanged-root check still walks every directory to compare mtimes; it only
  skips file parsing. Document this in `scan.rs` and consider an explicit
  `--rescan`/force hint in settings if launch traversal becomes hot.

---

## Phase F — UI dataflow & notification scope

### F1. Reduce `ConfigState` notification scope
- **Current:** every `Themed` view observes the whole `ConfigState`; changing
  scan roots/visualizer/window settings re-renders every themed view.
- **Change (choose one):**
  - Split appearance (`theme`, `dynamic_base`, `fonts/sizes`, `texture`) from
    other settings into two entities; `Themed` observes only appearance.
  - Or add `theme_revision` to `ConfigState`, bump it only on appearance changes,
    and have `Themed` skip recomputation/notify when it hasn’t moved.
- **Quick win:** `set_font`/`set_font_size` in `config_state.rs` currently always
  persist + notify even when unchanged; add an equality check like the other
  setters.

### F2. Decouple cross-view requests from library state
- **Current:** `LibraryState.requests: VecDeque<Request>` + `take_request()`,
  drained by `TabsView` only when `library_dirty`.
- **Change:** introduce a small `ActionBus`/`Requests` entity (or typed
  callbacks) that views publish to and `TabsView` drains. `LibraryState` then
  contains only library + scan state.
- **Acceptance:** “Go to playlist/artist/folder”, song menu, new-playlist still
  work; draining is not tied to `library_dirty`.

### F3. Avoid cloning add-songs results each render
- **Current:** `src/ui/playlist.rs` does `self.add_results.clone().into_iter()` in
  `render`.
- **Change:** iterate by index or clone only the clicked row’s data in the
  listener. `add_results: Vec<AddResult>` with `SharedString`/`Arc<str>` fields.

### F4. Drop duplicate `AlbumSection.year`
- **Current:** `src/ui/albums.rs` stores both `date: Option<ReleaseDate>` and
  `year: String`.
- **Change:** keep `date`; render `date.map(|d| d.year().to_string())` (or a
  cached label) at the two render sites. Update tests that assert `section.year`.

### F5. Coarse title updates in `TabsView`
- **Current:** `TabsView` observes playback and re-renders/notifies on every
  playback notification just to update the window title.
- **Change:** cache `(current SongId, ended)`; only update `last_title` and
  notify when that pair changes (or when the title string actually changes).

### F6. (Optional) Shared section/display cache across tabs
- Browse, search, playlist and queue each rebuild `AlbumSection`s from the same
  library revision. If several tabs are open, cache sections keyed by
  `(revision, scope, query)` in `LibraryState` or a dedicated view-model entity.
- Do this only after C1, and measure first.

---

## Phase G — Merge / deprecate cleanups

- [x] Merge the three search helpers around one `Library::grouped` entry point (C6).
- [x] Merge rail text filtering into `model::search` (C4).
- [x] Merge the two JSON writers into `model::persist` (D2).
- [x] Merge `collect_dirs` + `collect_files` (E1).
- [x] Replace `fonts` + `font_sizes` with one `ContainerStyle` map (D5).
- [x] Remove `PlaylistId(0)` queue sentinel (A4).
- [x] Replace `SongId = u64` with a newtype (A1).
- [ ] Resolve `Playlist::cover()` intent: either add
      `cover: Option<SongId>` to `PlaylistKind::Custom` (persist it, use it in
      `albums::section`) or delete the misleading “explicit cover wins” comment
      (`src/model/playlist.rs`, `src/ui/albums.rs`).

---

## Suggested commit sequence

1. **A1 + A2 + A3 + A4 + A5** — foundational type/queue safety.
2. **B1 + B2 + B3 + B4 + B6** — mutation batching, cached views, comparator
   allocations, hash membership, `Copy` stats.
3. **D1 + D2 + D3 + D4** — persistence versions, shared atomic writer,
   deterministic output, id-keyed cache.
4. **C2 + C3 + C4 + C5 + C6** — search index/normalization, `FieldSet`, unified
   matching, renames, wrapper collapse.
5. **E1 + E2** — single scan walk and canonicalization cleanup.
6. **C1 + F3 + F4 + F5** — presentation caches and render-path allocations.
7. **F1 + F2 + F6 + B5 + B7 + C7 + D5 + D6** — architecture/interning/optional
   refactors, one at a time with measurements.

---

## Tests / acceptance checklist (copy into PRs)

- [ ] `SongId` serde round-trips through `stats.json` and `playlists.json`
      (old files still load, or version bump documented).
- [ ] Same file scanned via a nested root produces one stable `SongId`.
- [ ] `LibraryState::queue_entries` is the only id→path resolver; queue still
      plays command-line files not in the library.
- [ ] No `PlaylistId(0)` remains (`grep -R "PlaylistId(0)" src`).
- [ ] Library reindex counter is 1 per scan and 1 per bulk insert.
- [ ] `playlists()`, `custom_playlists()`, `smart_playlists()` no longer sort/
      scan per call.
- [ ] Bulk add/remove of 10k ids does not regress.
- [ ] Two identical saves produce identical bytes.
- [ ] Song-schema version bump does not invalidate playlists/stats.
- [ ] Rail filter behavior documented and shares one matcher with search.
- [ ] Search over a large library does not re-lowercase metadata per keystroke.
- [ ] Queue drag/reorder, play-index, remove-current semantics unchanged.
- [ ] Theme/font change still live-updates; adding a scan root doesn’t re-theme
      unrelated views (or the chosen revision check works).
- [ ] `cargo test --all-targets` and `cargo clippy` clean.

---

# Session log — implementation status (append-only)

> This section records what was actually implemented, what remains, and the
> useful parts of a later review. Treat earlier phases as the original design
> intent; this section is the current source of truth for next steps.

## Completed in this session

### Phase A — done
- **A1 `SongId` newtype.** `SongId(pub u64)` with `#[serde(transparent)]`,
  `get()`, `From<u64>`; `generate_song_id` wraps the hash. `CoverCache` is now
  keyed by `SongId`. Old numeric JSON keys remain wire-compatible; tests added
  in `identity.rs` and `stats.rs` (old-shape stats JSON).
- **A2 `QueueEntry`.** Added `src/model/queue.rs`; all queue plumbing in
  `audio.rs`, `playback_state.rs`, `library_state.rs`, `menu.rs`, `queue.rs`
  uses it. No `(SongId, PathBuf)` queue tuples remain.
- **A3 `LibraryState::queue_entries`.** Added as the single id→path resolver;
  used by `play`, `menu.rs`, and `ensure_songs`.
- **A4 `SectionSource`.** `AlbumSection` now has
  `source: SectionSource::{Playlist(PlaylistId), Queue}`; `row_target`,
  `RowActions`, `album_list::play`, and queue rows use it. No `PlaylistId(0)`.
- **A5 `MenuOrigin`.** `SongMenuRequest { songs, position, origin }` with
  `MenuOrigin::{Row { playlist }, Queue, Card(id)}`.

### Phase B — partially done
- **B1 batching.** `Library::batch()` returns a `LibraryBatch` RAII guard;
  nested `reindex()` is suppressed and one rebuild runs on drop.
  `Library::insert_songs()` exists. Used by `scan::sync`, `merge_scanned`, and
  `ensure_songs`. `reindex_count` counter + test.
- **B2 cached views.** `Index` now caches `sorted_playlists`, `custom_playlists`,
  `smart_by_kind`; `playlists()`, `custom_playlists()`, `smart_playlists()` read
  those instead of scanning/sorting per call. `rebuild_smart` refreshes only the
  smart cache.
- **B3 comparator allocations.** `display_title` returns `Cow<str>`;
  `title_of` returns `&str`; `SmartKind::select` decorates with
  `(song, stats, title)`; `rebuild_auto` uses `sort_by_cached_key`.
- **B4 hash membership.** Bulk playlist add/remove uses `HashSet` past 8 items.
- **B6 `SongStats: Copy`; `Stats::get` uses `.copied()`.
- **B5, B7 not done.**

### Phase C — partially done
- **C3 `FieldSet`** (`u8` bitset) replaces `Vec<Field>` for query/needed fields.
- **C4 `TextMatcher`** added to `model::search`; rail `query_matches` deleted and
  rail filters use `TextMatcher`.
- **C5 `AlbumGroup` renamed `SongGroup`.
- **C6 wrappers collapsed:** `all_albums` and `matching_songs` deleted; callers
  use `Library::select` / `select_songs`; one `search` wrapper remains.
- **C1, C2, C7 not done.**

### Phase D — partially done
- **D1 per-file versions:** `CACHE_VERSION`, `PLAYLISTS_VERSION`,
  `STATS_VERSION`; tests for playlist/cache independence.
- **D2 `src/model/persist.rs`:** `read_json`, `write_json_atomic`; used by
  `scan.rs` and `Config`.
- **D3 deterministic output:** cache `dirs` is `BTreeMap`, songs sorted by
  `SongId`, config `fonts`/`font_sizes` are `BTreeMap`; deterministic-save test.
- **D4 song cache keyed by `SongId`** with a temporary path→id map during sync.
- **D5, D6 not done.**

### Phase E — partially done
- **E1 single filesystem walk:** `collect_tree` returns directory stamps and
  audio files in one pass; `collect_dirs`/`collect_files` deleted.
- **E2, E3 not done.**

### Phase F — partially done
- **F1 quick win:** `set_font`/`set_font_size`/`reset_font_size` skip persist +
  notify when unchanged. Full appearance-revision split not done.
- **F3:** playlist add-results iterate by reference and use `SharedString`.
- **F4:** `AlbumSection.year` removed; labels derive from `date` at render.
- **F2, F5, F6 not done.**

### Verification
- `cargo build --all-targets` clean.
- `cargo test --all-targets`: 231 unit tests + 10 layout tests pass.
- `cargo clippy` is unavailable in this toolchain.

### LOC note
The working tree is currently **net-positive (~+1,650 / −815)**. If the goal is a
net-negative diff, the next session must be removal-focused. The highest-yield
deletions are listed below; do not add optional caches/interners until those
land.

---

# Next session — useful review findings (prioritized)

> These are the parts of a later review worth acting on. Items already done
> above must NOT be redone.

## P0 — correctness fixes (small, safe)

1. **Playlist id collision on rescan.**
   `scan::sync` builds a fresh `Library::default()`, so `next_playlist_id`
   starts at 0 and album playlists take 0..n. `merge_scanned` then calls
   `adopt_custom_from`, which inserts persisted custom ids (3, 4, …), silently
   overwriting albums with the same ids. Album ids also churn every rescan,
   breaking open album tabs.
2. **Panics on user input.**
   `search::parse_duration` and `lyrics::parse_timestamp_tag` call
   `Duration::from_secs_f64`, which panics on NaN/inf/negative. Inputs like
   `dur:nan`, `dur:inf`, `dur:0:-5`, `[00:nan]` panic the search box on every
   keystroke. Use `Duration::try_from_secs_f64` and degrade to `Err`/`None`.
3. **Untagged files lose bitrate.** The no-tag branch of
   `SongMetadata::load` hardcodes `nominal_bitrate: None` even though
   `properties` was already read. Use the same value as the tagged branch.
4. **Duplicate queue entries share an element id.** Rows use
   `("album-track", song.get())`; the same song queued twice collides. Include
   the queue row/index in the id, not just the song.

## P1 — biggest structural/LOC wins

1. **Scan produces songs, not a `Library`.**
   Make `sync` return `HashMap<SongId, Arc<SongMetadata>>` + `SyncReport`, then
   update the live library with `replace_songs` → `prune` → `rebuild_auto` →
   `rebuild_smart`.
   Deletes `merge_scanned`, `adopt_custom_from` (and its test), the
   previous-library dance, and fixes the P0 collision. `seed_added` can take the
   song map.
2. **Merge duplicated row rendering.**
   `compact_row`/`track_row` share ~90%; `title_cell`/`artist_cell`,
   `bitrate_cell`/`duration_cell`/`year_cell`, and the two thumbnail helpers are
   near-identical. Collapse to `text_cell`, `right_cell`, one `track_row` with a
   compact/plain variant, and a `rounded: bool` thumbnail. Replace the repeated
   “clone actions, make listener” closures and multi-argument row functions with
   a `RowHandlers` struct. This is the largest pure-LOC cut.
3. **`SongMetadata: Default` + `#[default]` on `Lyrics::None`.**
   Removes ~10 hand-written 13-field literals in tests and collapses the
   no-tag branch of `load` into `..Default::default()`.
4. **Remove the batch guard once scanning updates in place.**
   If the index only rebuilds in `replace_songs`, `prune`, and `rebuild_auto`,
   keep `insert_songs` but delete `LibraryBatch`, `batch_depth`,
   `reindex_dirty`, and `reindex_count`. Re-check the “one reindex per scan/bulk
   insert” acceptance manually or with a dedicated test.
5. **Shrink the `Library` index.** Index only album-derived data
   (`song_album`, `playlist_release`, artist→albums) and rebuild it only on
   song/album changes. Keep `custom_playlists`/sorted album lists in small
   caches updated on playlist mutations, or sort custom playlists on demand.
   This removes the “custom edit re-walks every song” cost. Do not simply delete
   B2’s cached getters; preserve the “no scan per call” acceptance.
6. **Pure `PlayQueue` in `model/queue.rs`.**
   Move `entries`, `current`, remove/reorder arithmetic out of
   `AudioPlaybackController` into a unit-testable model type. Use a `HashSet`
   for bulk removal. This also makes the duplicate-entry element-id fix easier
   (queue position is model state).

## P2 — useful, medium risk

1. **Deterministic playlist ids — with disjoint id spaces.**
   Album id = stable hash of `PlaylistOrigin`; smart ids = three reserved
   constants; counter only for custom/temporary. Deletes origin matching,
   `smart_by_kind`, and id churn. **Caveat:** hashes, reserved ids, and the
   counter can collide. Use tagged high-bit ranges or an enum-like layout; do
   not assume hashes never collide with counter ids.
2. **Split `HashMap<PlaylistId, Playlist>`** into albums / custom (`BTreeMap`) /
   ephemeral. Removes the repeated `!is_temporary && !is_smart` filters. Do after
   deterministic ids.
3. **`TrackRow` holds `Arc<SongMetadata>` + number; format at render.**
   Removes four `String` copies per row per rebuild and mostly replaces C1.
4. **Bundle shared entities.** `#[derive(Clone)] struct App { library, playback,
   covers, config, animator }`; add `new_browse()` and one `open(TabId, make)`
   in `tabs.rs` to replace the three copy-pasted `open_*` bodies.
5. **Replace `Request` queue with `gpui` events.** `impl
   EventEmitter<Request> for LibraryState` + `cx.subscribe_in` in `TabsView`.
   Deletes `requests`, `take_request`, the polling block, and `library_dirty`.
6. **Move `selected` out of `LibraryState`.** It is active-tab state; tab
   switches currently notify every library observer.
7. **Split library notifications** into Songs / Playlists / Stats. `record_play`
   should not make every album list rebuild every ~30 s.
8. **`RowLayout` enum + single `flatten(layout)`.** Replaces `set_flat`,
   `set_slim`, `set_compact_singles`, and the three `flatten*` functions.
   Group the five loose icon-grid fields into `IconGrid`.
9. **`Versioned<T>` envelope** for the three cache files; collapse the repeated
   version-plus-filter code. Delete the version changelog comment on
   `CACHE_VERSION`; version control history already has it.
10. **Config shape.** `fonts` + `font_sizes` → one
    `BTreeMap<String, ContainerStyle { font: Option<FontKind>, size: Option<f32> }>`
    (D5). Add `#[serde(default)]` on the struct and drop per-field attributes.
    Collapse the six “old config without X” tests into one `{}` test.
    `ThemeKind::Dynamic`/`DynamicBase` can become `base: Dark|Light` +
    `dynamic: bool`, but this is optional and touches many call sites.
11. **Normalize search needles once in `parse`.** `Matcher` still calls
    `matches_text`, which re-trims/lowercases the needle per song/term even
    though `TextMatcher` exists. Cache lowercased artist/album/title in the
    library index only after the search/index churn settles.
12. **Centralize smart-playlist presentation** (title, glyph, empty hint) on
    `SmartKind` instead of scattering it between model, rail, and
    `playlist.rs`.

## P3 — small cleanups / low priority

- Merge identical `dir_modified` and `file_modified`.
- `Group::hash_key` → `group as u64`.
- `albums_under` with `BTreeMap`.
- `Stats::prune` takes a predicate, breaking the stats↔library module cycle.
- `looks_like_lrc` can likely be `!parse_lrc(s).is_empty()`, but this is a
  parser-behavior change for little gain.
- `QueueView.total_secs` is written but never read (dead); remove it.
- `QueueView` observes playback twice (via `album_list::observe` and directly);
  keep one.
- Cache the queue total in `PlaybackState` so `LyricsView` and `QueueView` don’t
  both re-sum durations.

## Deferred / deliberately rejected for now

- **Lyrics out of `SongMetadata`.** Big memory/startup win, but a large dataflow
  and cache-format project, not a LOC win. Do it separately, with background
  lyrics loading and a lazy lyrics search index.
- **Re-key the song cache by `PathBuf`.** A later review suggested this, but D4
  asked for `SongId` keys. If scan-returns-songs lands, the temporary `by_path`
  map disappears anyway; keep `SongId` keys unless D4 is explicitly revisited.
- **Replace the hand-rolled FFT with `realfft`.** Faster/cleaner but adds a
  dependency and is unrelated to this refactor.
- **Macro-generating `ALL`/`label()` for enums.** Saves few lines, hurts
  debugging.
- **Blind `pub(crate)` dead-code pass.** `src/ui/main.rs` is a separate bin
  crate using `larkspur::…`; narrowing visibility can break it. Run
  `cargo check` and keep what the bin needs public.
- **Optional C1/C2/C7/D5/D6/E2/F2/F5/F6 from the original plan** are still not
  done; treat them as separate, measured changes.

## Suggested order for next session

1. P0 bugs.
2. Scan-returns-songs; delete `merge_scanned`/`adopt_custom_from`.
3. Deterministic playlist ids (disjoint ranges).
4. `SongMetadata: Default`; UI row/render merge.
5. `Versioned<T>`, event bus, move `selected`, `App` bundle.
6. Shrink index + remove batch guard.
7. `PlayQueue` extraction, queue-total cleanup.
8. Search normalization/caches, config shape, presentation cleanup.
9. Only then measure C1/C2/lyrics and other perf work.

## Acceptance for the next session

- No `PlaylistId` collision after rescans; album ids stable across rescans.
- Fuzz/edge tests for `dur:` and timestamp parsing never panic.
- Untagged files report the same bitrate as tagged ones.
- Duplicate queue entries render and reorder independently.
- `cargo test --all-targets` and `cargo build --all-targets` clean.
- Target a **net-negative or flat LOC diff**; prefer deleting code over adding
  abstractions unless the abstraction removes more than it adds.

---

# Session log — P0 fixes + scan-returns-songs (done)

- **P0.1 id collision:** `scan::sync` now returns
  `(HashMap<SongId, Arc<SongMetadata>>, SyncReport)` instead of mutating a
  throwaway `Library`. `LibraryState::adopt_scanned` applies the songs to the
  live library in one batch (`replace_songs` → `prune` → `rebuild_auto`), so
  custom playlists and the id counter survive and album ids can no longer alias
  them. `merge_scanned` and `Library::adopt_custom_from` are deleted; the old
  test is replaced by `rescanned_songs_keep_custom_playlists_and_album_ids_disjoint`.
  `seed_added` now takes song refs. `probe` applies the song map directly.
- **P0.2 parsing panics:** `search::parse_duration` and
  `lyrics::parse_timestamp_tag` use `try_from_secs_f64` and reject non-finite /
  negative values; tests cover `nan`, `inf`, negative components, and ranges.
- **P0.3 untagged bitrate:** the no-tag branch of `SongMetadata::load` now uses
  the already-read `nominal_bitrate`.
- **P0.4 duplicate queue rows:** row element ids now use the flattened list
  index (`item_ix`) rather than the song id, so the same song queued twice no
  longer shares an element id.
- Verification: `cargo build --all-targets` clean; `cargo test --all-targets`
  233 lib tests + 10 layout tests pass.
- Next per the order above: deterministic playlist ids (disjoint ranges), then
  `SongMetadata: Default` / row-render merge, then `Versioned<T>` / event bus.

# Session log — deterministic ids, defaults, versioned persistence (done)

- **P2.1 deterministic playlist ids with disjoint ranges.** `PlaylistId` now
  reserves three ranges: custom/temporary `0..2^62` from the persisted counter,
  albums `2^63..` from an FNV-1a hash of `PlaylistOrigin` (`PlaylistOrigin::id`),
  and smart playlists in the top three `u64` values (`SmartKind::id`).
  `rebuild_auto` inserts/updates by id and drops vanished albums in one retain;
  the origin-matching map is gone. `rebuild_smart` allocates nothing. Added
  `playlist_ids_are_stable_and_range_disjoint`.
- **Rescan temporary-playlist parity.** `Library::drop_temporary` is called by
  `LibraryState::adopt_scanned`, restoring the old “a rescan invalidates ad-hoc
  views” behavior now that songs are applied in place.
- **P1.3 `SongMetadata: Default`.** Added `Default` to `SongMetadata`, `SongId`,
  and `Lyrics` (`#[default] None`); the no-tag `load` branch and test helpers
  now use `..Default::default()` (net line reduction).
- **P2.9 `Versioned<T>`.** `persist::read_versioned` / `write_versioned` use a
  JSON object with a `version` field beside the body, preserving the existing
  wire shape. `CacheFile`/`PlaylistsFile`/`StatsFile` lost their own `version`
  fields; one version-mismatch test added.
- **P3 quick cleanups.** Removed dead `QueueView.total_secs`; dropped the
  duplicate playback observation (album-list observe already covers it); merged
  `dir_modified`/`file_modified` into `modified`; replaced `Group::hash_key`
  with `#[repr(u64)]` + `group as u64`.
- Verification: `cargo test --all-targets` → 235 lib tests + 10 layout tests
  pass; build clean, no warnings.
- Next: row-render merge (`compact_row`/`track_row`, `RowHandlers`) and
  `PlayQueue` extraction, or the smaller `TrackRow` → `Arc<SongMetadata>`
  change that mostly subsumes C1.

# Session log — queue model, row merge, small cleanups (done)

- **P1.6 `PlayQueue` in `model/queue.rs`.** The entry list, loaded index and
  generation counter moved out of `PlaybackController` into a pure
  `PlayQueue` with `Removal::{Unchanged,Shifted,Reload,Stop}` outcomes.
  Remove/reorder arithmetic is unit-tested (5 tests: move follows the loaded
  track, surviving-current shift, reload-next, stop/empty, generation). The
  controller now only drives track loading around it; its remove branch shrank
  from ~45 lines to a 3-arm match. Bulk removal builds a `HashSet` past 8 ids.
- **P1.2 row-render merge.** `compact_row` and `track_row` are one function
  with a compact/plain variant and a single `Option<String>` year slot; the six
  row listeners are bundled in a `RowHandlers` struct. `title_cell`/`artist_cell`
  share `text_cell`; `bitrate_cell`/`duration_cell`/`year_cell` share
  `right_cell`; `thumbnail`/`thumbnail_rounded` share `thumbnail_sized`.
- **P3 `Stats::prune` takes a predicate** (`impl FnMut(SongId) -> bool`),
  breaking the stats→library module dependency; call sites pass
  `|id| library.get(id).is_some()`.
- **P3 `albums_under` uses a `BTreeMap`** instead of collecting + sorting.
- **P2.11 matcher needles normalized once.** `Query::matcher` pre-trims and
  lowercases each term; `matches_text` and its per-song lowercase allocation are
  gone (`matches_term` takes the prepared needle).
- Verification: `cargo build --all-targets` clean (no warnings);
  `cargo test --all-targets` → 240 lib tests + 10 layout tests pass. Clippy is
  still unavailable in this toolchain.
- Still open (unchanged priority): `TrackRow` → `Arc<SongMetadata>` (P2.3),
  `App` bundle (P2.4), gpui `EventEmitter` request bus (P2.5), `selected` out of
  `LibraryState` (P2.6), split library notifications (P2.7), `RowLayout` enum
  (P2.8), config `ContainerStyle` map (P2.10/D5), smart-playlist presentation
  (P2.12), deterministic-id index shrink (P1.5), and optional C1/C2/C7/D6/E2/
  F2/F5/F6.

# Session log — Arc rows, RowLayout, config styles, appearance revision (done)

- **P2.3 `TrackRow` holds `Arc<SongMetadata>`.** `TrackRow` is now
  `{ song, number, meta }`; sections resolve songs with `Library::get_arc`, and
  the title/artist/duration cells format at render (only visible rows). The
  queue synthesizes minimal `SongMetadata` for command-line files not in the
  library. `section()` no longer clones four strings per row, and
  `album_artists`/`custom_artist` take `&[Arc<SongMetadata>]`.
- **P2.8 `RowLayout` enum.** `RowLayout::{Albums, Flat, CompactSingles}` replaces
  `set_flat`/`set_compact_singles` and the three `flatten*` functions; one
  `albums::flatten(sections, layout)` serves browse, queue and search. The
  row-list's two booleans collapsed to one `layout` field.
- **P2.10 / D5 config styles.** `ContainerStyle { font, size }` and a single
  `styles: BTreeMap<String, ContainerStyle>`. `Config` now derives `Serialize`
  and has a hand-written `Deserialize` (`ConfigWire`) that folds the legacy
  `fonts`/`font_sizes` maps into `styles`, so old config files still load while
  saves write only the new shape. Empty style entries are dropped. Added tests
  for legacy migration and override clearing. `font_override`/`font_size_override`
  give `ConfigState` cheap change checks.
- **P2.12 smart-playlist presentation.** `SmartKind::glyph()` and
  `SmartKind::empty_hint()` centralize labels the rail and playlist view used to
  duplicate.
- **P2.4 (partial) `new_browse` helper.** `TabsView` builds browse tabs through
  one helper instead of three copies of `BrowseView::new`.
- **F1 appearance revision.** `ConfigState::appearance_revision` bumps only on
  theme / dynamic base / accent / font / size changes; `Themed` skips its
  recompute + `notify` while the revision is unchanged, so scan-root, visualizer
  and window changes no longer re-theme every view.
- **E2 canonicalization.** `song_id_for_canonical` hashes an already-canonical
  path; the scanner resolves symlinks once per link during its walk and parses
  with `SongMetadata::load_canonical`, skipping a `canonicalize` syscall per
  file. Added an id-equivalence test.
- **E3 docs.** `scan::sync` documents that the "fast path" still walks every
  directory (it only skips re-parsing files).
- Verification: `cargo build --all-targets` clean (no warnings);
  `cargo test --all-targets` → 243 lib tests + 10 layout tests pass. Clippy
  unavailable in this toolchain.
- Remaining (architectural/optional): request event bus (F2/P2.5), move
  `selected` out of `LibraryState` (P2.6), split library notifications (P2.7),
  shrink index + remove batch guard (P1.4/P1.5), search/presentation caches
  (C1/C2), interning (C7), epoch-millis stats (D6), and `Playlist::cover()`
  intent.
