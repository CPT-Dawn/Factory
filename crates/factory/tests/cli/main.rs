//! Tests at the binary seam: argv in; stdout, stderr and exit code out.
//!
//! Every test builds a [`support::Sandbox`] and drives the built `factory` binary inside it.
//! Add a module per command here.

mod devlab_golden;
mod harness;
mod help;
mod log;
mod review;
mod router;
// Marked cfg(test) so clippy treats the harness as test code and lets it unwrap.
#[cfg(test)]
mod support;
