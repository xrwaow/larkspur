/// A user intent, decoupled from any specific input backend.
///
/// Key events (GPUI, or a terminal's `KeyCode` if a TUI ever lands) map
/// *into* this enum, and handlers match on it — so the "which backend"
/// question stays isolated to the mapping function instead of leaking
/// into every view's key handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InputAction {
    NextTrack,
    PrevTrack,
    TogglePause,
    SeekForward,
    SeekBackward,
    /// Move the playlist selection down a row.
    SelectNext,
    /// Move the playlist selection up a row.
    SelectPrev,
    /// Scroll the active list down by a page.
    PageDown,
    /// Scroll the active list up by a page.
    PageUp,
    /// Play the selected row.
    Activate,
    /// Switch to the next center tab.
    NextTab,
    /// Switch to the previous center tab.
    PrevTab,
    /// Toggle the search box, bringing the search panel into view.
    ToggleSearch,
    /// Close the active tab (or the search panel, if it's up).
    CloseTab,
    /// Dismiss whatever overlay is on top.
    CloseOverlay,
}
