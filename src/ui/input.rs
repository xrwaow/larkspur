//! Key-name mapping — the one place GPUI key names are turned into a
//! backend-agnostic [`InputAction`].
//!
//! Everything downstream matches on `InputAction`, so swapping or extending the
//! input backend (say, a TUI) is a change to this one function. It lives here
//! rather than in a view because the tab container and every list container
//! call it, not just the transport.

use gpui::Keystroke;

use crate::model::InputAction;

/// Map a key press to a backend-agnostic [`InputAction`].
///
/// Views call this for every key they receive and act on the subset they own;
/// the rest bubble up.
pub fn action_for_key(keystroke: &Keystroke) -> Option<InputAction> {
    let key = keystroke.key.as_str();
    let modifiers = keystroke.modifiers;
    match key {
        // Tab switching, closing, and search are the tab container's.
        "tab" if modifiers.control => Some(if modifiers.shift {
            InputAction::PrevTab
        } else {
            InputAction::NextTab
        }),
        "w" if modifiers.control => Some(InputAction::CloseTab),
        "f" if modifiers.control && modifiers.shift => Some(InputAction::ToggleSearch),
        "f" if modifiers.control => Some(InputAction::FocusSearch),
        "escape" => Some(InputAction::CloseOverlay),
        "left" => Some(InputAction::SeekBackward),
        "right" => Some(InputAction::SeekForward),
        "space" => Some(InputAction::TogglePause),
        "up" => Some(InputAction::SelectPrev),
        "down" => Some(InputAction::SelectNext),
        "pageup" => Some(InputAction::PageUp),
        "pagedown" => Some(InputAction::PageDown),
        "enter" => Some(InputAction::Activate),
        _ => None,
    }
}
