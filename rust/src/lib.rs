//! Rust port of thesh — library crate.
//!
//! Modules mirror the C sources in `../src/` one-to-one (utf8.c → utf8,
//! history.c → history, …) so the differential harness and the golden
//! vectors can track parity milestone by milestone.

pub mod utf8;
pub mod util;
pub mod term;
pub mod spell;
pub mod dict;
pub mod history;
pub mod keys;
pub mod config;
pub mod bind;
pub mod presets;
pub mod screen;
pub mod exec;
pub mod editor;
pub mod mux;
pub mod run;
