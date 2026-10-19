//! Queue entries — one playable item in the playback queue — and the pure,
//! unit-testable [`PlayQueue`] that holds them.
//!
//! The queue is a `(SongId, PathBuf)` pair everywhere: the id is what the UI
//! resolves metadata and stats by, the path is what the decoder opens. Naming
//! the pair gives the audio layer a model-owned type to speak (the model sits
//! below the audio and UI layers, so this can't live there).
//!
//! [`PlayQueue`] owns the entry list, the currently-loaded index and the
//! change counter. It knows nothing about decoding, so its remove/reorder
//! arithmetic can be tested without an audio device; the controller drives
//! loading around it.

use std::collections::HashSet;
use std::path::PathBuf;

use super::identity::SongId;

/// Above this many ids, bulk removal builds a hash set instead of scanning.
const HASH_THRESHOLD: usize = 8;

/// One entry in the playback queue: the song identity plus the file to open.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QueueEntry {
    pub song: SongId,
    pub path: PathBuf,
}

/// What a [`PlayQueue::remove`] call did to the loaded track, so the
/// controller knows whether it must restart, stop, or just follow the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removal {
    /// Nothing changed (no ids matched).
    Unchanged,
    /// The loaded track survived; `current` was re-pointed at its shifted slot.
    Shifted,
    /// The loaded track was removed and the entry now occupying `current`
    /// must be loaded.
    Reload,
    /// The loaded track was removed with nothing after it: stop the player.
    /// `current` is the last surviving entry, or `None` when the queue emptied.
    Stop,
}

/// The play queue: entries, the loaded index, and a generation counter bumped
/// on every content change.
#[derive(Debug, Default, Clone)]
pub struct PlayQueue {
    entries: Vec<QueueEntry>,
    current: Option<usize>,
    generation: u64,
}

impl PlayQueue {
    /// A new queue with `entries` and nothing loaded. Starts at generation 1 so
    /// a queue view built from it snapshots on its first render.
    pub fn new(entries: Vec<QueueEntry>) -> Self {
        Self { entries, current: None, generation: 1 }
    }

    pub fn entries(&self) -> &[QueueEntry] {
        &self.entries
    }

    pub fn current(&self) -> Option<usize> {
        self.current
    }

    /// The queue's generation — bumped on every content change. The UI's cheap
    /// "did the queue move" check.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<&QueueEntry> {
        self.entries.get(index)
    }

    /// The loaded entry, if any.
    pub fn current_entry(&self) -> Option<&QueueEntry> {
        self.current.and_then(|i| self.entries.get(i))
    }

    /// Replace the whole queue and mark `current` as loaded.
    pub fn replace(&mut self, entries: Vec<QueueEntry>, current: usize) {
        self.entries = entries;
        self.current = Some(current);
        self.generation += 1;
    }

    /// Record which entry is loaded (after a play-index or advance).
    pub fn set_current(&mut self, current: Option<usize>) {
        self.current = current;
    }

    /// Append entries, returning the index of the first appended one.
    pub fn extend(&mut self, entries: Vec<QueueEntry>) -> usize {
        let first = self.entries.len();
        self.entries.extend(entries);
        self.generation += 1;
        first
    }

    /// Drop every entry. The loaded track keeps playing; the queue just won't
    /// advance past it, and anything appended afterwards starts from its first
    /// entry.
    pub fn clear(&mut self) {
        self.generation += (!self.entries.is_empty()) as u64;
        self.entries.clear();
        self.current = None;
    }

    /// Move the entry at `from` to `to`, keeping the loaded track on its entry.
    /// Returns whether the queue changed.
    pub fn move_entry(&mut self, from: usize, to: usize) -> bool {
        if from == to || from >= self.entries.len() || to >= self.entries.len() {
            return false;
        }
        let entry = self.entries.remove(from);
        self.entries.insert(to, entry);
        self.generation += 1;
        if let Some(current) = self.current {
            self.current = Some(if current == from {
                to
            } else if from < current && to >= current {
                current - 1
            } else if from > current && to <= current {
                current + 1
            } else {
                current
            });
        }
        true
    }

    /// Drop every entry whose song is in `ids`. Updates `current` as described
    /// by the returned [`Removal`].
    pub fn remove(&mut self, ids: &[SongId]) -> Removal {
        let before = self.entries.len();
        if before == 0 {
            return Removal::Unchanged;
        }
        let matches = |entry: &QueueEntry| ids.contains(&entry.song);
        // Snapshot the loaded slot's fate before the prefix shifts.
        let loaded = self.current.and_then(|i| self.entries.get(i)).map(|e| e.song);
        let removed_before =
            self.current.map_or(0, |c| self.entries[..c].iter().filter(|e| matches(e)).count());

        if ids.len() > HASH_THRESHOLD {
            let set: HashSet<&SongId> = ids.iter().collect();
            self.entries.retain(|entry| !set.contains(&entry.song));
        } else {
            self.entries.retain(|entry| !matches(entry));
        }
        if self.entries.len() == before {
            return Removal::Unchanged;
        }
        self.generation += 1;

        let Some(current) = self.current else {
            return Removal::Unchanged;
        };
        let loaded_removed = loaded.is_some_and(|id| ids.contains(&id));
        if loaded_removed {
            if current < self.entries.len() {
                // The next surviving entry took the removed slot: load it.
                self.current = Some(current);
                Removal::Reload
            } else {
                // Nothing after it: stop, landing on the last survivor (if any).
                self.current =
                    if self.entries.is_empty() { None } else { Some(self.entries.len() - 1) };
                Removal::Stop
            }
        } else {
            // The loaded track survived: re-point at its shifted slot.
            self.current = Some(current - removed_before);
            Removal::Shifted
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn entry(id: u64) -> QueueEntry {
        QueueEntry { song: SongId(id), path: PathBuf::from(format!("/m/{id}.flac")) }
    }

    fn ids(q: &PlayQueue) -> Vec<u64> {
        q.entries().iter().map(|e| e.song.get()).collect()
    }

    #[test]
    fn move_entry_follows_loaded_track() {
        let mut q = PlayQueue::new((0..4).map(entry).collect());
        q.set_current(Some(1));
        assert!(q.move_entry(1, 3));
        assert_eq!(ids(&q), vec![0, 2, 3, 1]);
        assert_eq!(q.current(), Some(3));
        // Moving from after the loaded slot to before it shifts it right.
        assert!(q.move_entry(3, 0));
        assert_eq!(ids(&q), vec![1, 0, 2, 3]);
        assert_eq!(q.current(), Some(0));
    }

    #[test]
    fn remove_surviving_current_shifts_index() {
        let mut q = PlayQueue::new((0..4).map(entry).collect());
        q.set_current(Some(2));
        assert_eq!(q.remove(&[SongId(0)]), Removal::Shifted);
        assert_eq!(ids(&q), vec![1, 2, 3]);
        assert_eq!(q.current(), Some(1));
        assert_eq!(q.current_entry().unwrap().song, SongId(2));
    }

    #[test]
    fn remove_loaded_reloads_next_or_stops() {
        let mut q = PlayQueue::new((0..3).map(entry).collect());
        q.set_current(Some(1));
        assert_eq!(q.remove(&[SongId(1)]), Removal::Reload);
        assert_eq!(q.current_entry().unwrap().song, SongId(2));

        let mut q = PlayQueue::new((0..3).map(entry).collect());
        q.set_current(Some(2));
        assert_eq!(q.remove(&[SongId(2)]), Removal::Stop);
        assert_eq!(q.current_entry().unwrap().song, SongId(1));

        let mut q = PlayQueue::new(vec![entry(7)]);
        q.set_current(Some(0));
        assert_eq!(q.remove(&[SongId(7)]), Removal::Stop);
        assert_eq!(q.current(), None);
        assert!(q.is_empty());
    }

    #[test]
    fn remove_unknown_is_unchanged() {
        let mut q = PlayQueue::new((0..2).map(entry).collect());
        q.set_current(Some(0));
        assert_eq!(q.remove(&[SongId(99)]), Removal::Unchanged);
        assert_eq!(q.current(), Some(0));
    }

    #[test]
    fn generation_bumps_only_on_change() {
        let mut q = PlayQueue::new(vec![entry(0)]);
        let g = q.generation();
        assert_eq!(q.remove(&[SongId(9)]), Removal::Unchanged);
        assert_eq!(q.generation(), g);
        q.remove(&[SongId(0)]);
        assert!(q.generation() > g);
    }
}
