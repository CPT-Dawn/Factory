//! The factory's pure domain: no I/O, no clock, no processes.
//!
//! Time, files and processes come in as arguments, so every rule here is a plain function
//! that table tests can drive.

pub mod duration;
pub mod event;
pub mod exit;
pub mod hook;
pub mod lab;
pub mod lead;
pub mod quota;
pub mod review;
pub mod timestamp;
pub mod wait;
