/// A user intent, decoupled from any specific input backend.
///
/// Key events (GPUI, or a terminal's `KeyCode` if a TUI ever lands) map
/// *into* this enum, and handlers match on it — so the "which backend"
/// question stays isolated to the mapping function instead of leaking
/// into every view's key handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InputAction {
    Quit,
    NextTrack,
    PrevTrack,
    TogglePause,
    SeekForward,
    SeekBackward,
    VolumeUp,
    VolumeDown,
    FocusLeftPane,
    FocusRightPane,
}
