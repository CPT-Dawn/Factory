//! The factory's pure domain: no I/O, no clock, no processes.
//!
//! Time, files and processes come in as arguments, so every rule here is a plain function
//! that table tests can drive.

pub mod agy;
pub mod bar;
pub mod ci;
pub mod claude;
pub mod codex;
pub mod cutover;
pub mod duration;
pub mod event;
pub mod exit;
pub mod finished;
pub mod fleet;
pub mod hook;
pub mod lab;
pub mod lead;
pub mod quota;
pub mod review;
pub mod status;
pub mod steward;
pub mod tier;
pub mod timestamp;
pub mod usage;
pub mod wait;
