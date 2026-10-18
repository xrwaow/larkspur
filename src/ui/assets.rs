//! The app's embedded SVG icons.
//!
//! gpui's `svg()` element asks the app's [`AssetSource`] for an icon by its
//! path. The transport and shuffle glyphs ship as SVGs in `assets/`, so we
//! compile them into the binary here and serve them back by that path. Keeping
//! them embedded (rather than reading from disk) means a packaged binary finds
//! its icons no matter where it runs from.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

pub struct Assets;

impl Assets {
    /// Every asset we serve, keyed by the path passed to `svg().path(...)`.
    const FILES: &'static [(&'static str, &'static [u8])] = &[
        ("assets/repeat.svg", include_bytes!("../../assets/repeat.svg")),
        ("assets/repeat_one.svg", include_bytes!("../../assets/repeat_one.svg")),
        ("assets/shuffle.svg", include_bytes!("../../assets/shuffle.svg")),
    ];
}

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(Self::FILES
            .iter()
            .find(|(name, _)| *name == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let prefix = if path.is_empty() { "assets/" } else { path };
        Ok(Self::FILES
            .iter()
            .filter_map(|(name, _)| name.strip_prefix(prefix))
            .filter(|rest| !rest.contains('/'))
            .map(SharedString::from)
            .collect())
    }
}
