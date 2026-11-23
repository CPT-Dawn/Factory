//! `factory lab`, which also answers to the name `devlab`: leases of this PC's local stack
//! slots (#14). As `devlab` it speaks devlab's own dialect: its commands, its text and its exit
//! codes, byte for byte with the golden transcripts (#13) ([`cli`]). As `factory lab` the same
//! verbs keep the output contract ([`native`]).
//!
//! The CLI reads the configs, finds the owner, runs stacks and commands; the daemon keeps the
//! lease store and decides who is granted what ([`service`]). Phones and emulators (#16) are the
//! daemon's too: it follows one `adb track-devices` stream and boots and watches emulators.
//! Reservations (#17) and the watcher (#18) build on these modules beside them.

mod android;
mod cli;
mod context;
pub(crate) mod host;
mod lease;
mod native;
mod owner;
mod reservations;
mod reserve;
mod run;
pub mod service;
mod stack;
mod status;
mod store;
pub mod wire;

pub use cli::main;
pub use native::main as native;
