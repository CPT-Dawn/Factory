//! Tests at the binary seam: argv in; stdout, stderr and exit code out.
//!
//! Every test builds a [`support::Sandbox`] and drives the built `factory` binary inside it.
//! Add a module per command here.

mod agy;
mod ci;
mod devlab_golden;
mod harness;
mod help;
mod install;
// cfg(test) for the same reason as support: its helpers unwrap.
#[cfg(test)]
mod lab;
mod lead;
mod log;
mod reserve;
mod review;
mod router;
// Marked cfg(test) so clippy treats the harness as test code and lets it unwrap.
#[cfg(test)]
mod support;
