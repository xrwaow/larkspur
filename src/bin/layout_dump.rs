//! Print the app's workspace layout as text, without opening a window.
//!
//! Useful for eyeballing the layout in a terminal and for handing the
//! current UI structure to an LLM. The same output is what `tests/layout.rs`
//! asserts against.
//!
//!     cargo run --bin layout_dump

fn main() {
    print!("{}", larkspur::ui::layout::describe_app());
}
