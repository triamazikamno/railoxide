//! In-memory GPUI fixture for the `RailOxide` browser extension.
#[cfg(target_family = "wasm")]
mod browser;

#[cfg(target_family = "wasm")]
pub use browser::{run, stop};
