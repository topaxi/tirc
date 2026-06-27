// matrix-sdk's e2e-encryption code has deeply nested async fns; without a raised
// limit the compiler overflows while proving the sync loop future is `Send`.
#![recursion_limit = "256"]

pub mod backends;
pub mod config;
pub mod core;
pub mod lua;
pub mod tui;
pub mod ui;
