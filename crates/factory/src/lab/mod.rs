//! `factory lab`, which also answers to the name `devlab`: leases of this PC's local stack
//! slots (#14), in devlab's own dialect: its commands, its text and its exit codes, byte for byte
//! with the golden transcripts (#13).
//!
//! The CLI reads the configs, finds the owner, runs stacks and commands; the daemon keeps the
//! lease store and decides who is granted what ([`service`]). Devices (#16), reservations
//! (#17) and the watcher (#18) build on these modules beside them.

mod cli;
mod context;
mod host;
mod lease;
mod owner;
mod run;
pub mod service;
mod stack;
mod status;
mod store;
pub mod wire;

pub use cli::main;
