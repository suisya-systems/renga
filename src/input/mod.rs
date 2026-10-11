//! Input helpers split out of `src/app.rs` (Issue #66).
//!
//! Only the IME composition overlay lives here. The other input paths
//! stay on `App`, split across `src/app/keyboard_input.rs`,
//! `pointer_input.rs` and `sidebar_input.rs`.

pub mod overlay;
