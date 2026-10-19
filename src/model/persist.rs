//! Shared, crash-safe JSON persistence.
//!
//! Both the library cache (song metadata, playlists, stats) and the user config
//! are JSON files written through a temp file + rename, so a crash mid-write
//! can't leave a truncated file that fails to parse on the next launch. This
//! module is the one implementation of that pattern.

use std::fs;
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

/// Read `path` as `T` when its JSON object carries `version`.
///
/// The different cache files bump independently; centralizing the
/// read-and-check-version dance keeps a schema bump from touching every
/// loader. A mismatch reads as `None`, exactly like a missing or broken file.
pub fn read_versioned<T: DeserializeOwned>(path: &Path, version: u32) -> Option<T> {
    let mut value: Value = read_json(path)?;
    let object = value.as_object_mut()?;
    if object.get("version")?.as_u64()? != u64::from(version) {
        return None;
    }
    object.remove("version");
    serde_json::from_value(value).ok()
}

/// Write `value` as an object with a `version` field beside its fields.
pub fn write_versioned<T: Serialize>(
    path: &Path,
    value: &T,
    version: u32,
    pretty: bool,
) -> anyhow::Result<()> {
    let mut value = serde_json::to_value(value)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("versioned JSON body must be an object"))?;
    object.insert("version".into(), Value::from(version));
    write_json_atomic(path, &value, pretty)
}

/// Read and parse `path`, or `None` if it's missing, unreadable, or invalid.
///
/// A missing or broken cache/config is never fatal: callers treat `None` as a
/// first run and fall back to defaults.
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Option<T> {
    fs::read(path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

/// Serialize `value` to `path` atomically (temp file + rename).
///
/// `pretty` keeps a human-editable file (the config); the library cache and
/// stats use compact JSON to stay small.
pub fn write_json_atomic<T: Serialize>(
    path: &Path,
    value: &T,
    pretty: bool,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = if pretty {
        serde_json::to_vec_pretty(value)?
    } else {
        serde_json::to_vec(value)?
    };
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_replaces_atomically() {
        let dir = std::env::temp_dir().join("larkspur-persist-test");
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("v.json");

        write_json_atomic(&path, &vec![1u32, 2, 3], false).unwrap();
        let back: Vec<u32> = read_json(&path).unwrap();
        assert_eq!(back, vec![1, 2, 3]);

        // A second write replaces the file; no temp file is left behind.
        write_json_atomic(&path, &vec![9u32], true).unwrap();
        let back: Vec<u32> = read_json(&path).unwrap();
        assert_eq!(back, vec![9]);
        assert!(!path.with_extension("tmp").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn version_mismatch_reads_as_none_and_matching_version_round_trips() {
        let dir = std::env::temp_dir().join("larkspur-persist-versioned");
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("v.json");

        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Body {
            songs: Vec<u32>,
        }
        let body = Body { songs: vec![1, 2] };
        write_versioned(&path, &body, 3, false).unwrap();
        assert_eq!(read_versioned::<Body>(&path, 3), Some(body));
        assert_eq!(read_versioned::<Body>(&path, 4), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn broken_input_reads_as_none() {
        let dir = std::env::temp_dir().join("larkspur-persist-broken");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("broken.json");
        fs::write(&path, b"{not json").unwrap();
        let back: Option<Vec<u32>> = read_json(&path);
        assert!(back.is_none());
        let _ = fs::remove_dir_all(&dir);
    }
}
