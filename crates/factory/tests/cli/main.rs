//! Tests at the binary seam: argv in; stdout, stderr and exit code out.
//!
//! Every test builds a [`support::Sandbox`] and drives the built `factory` binary inside it.
//! Add a module per command here.

// cfg(test) for the same reason as support: its helpers unwrap.
#[cfg(test)]
mod adb_stream;
mod agy;
// cfg(test) for the same reason as support: its helpers unwrap.
#[cfg(test)]
mod bar;
mod ci;
// cfg(test) for the same reason as support: its helpers unwrap.
#[cfg(test)]
mod claude;
// cfg(test) for the same reason as support: its helpers unwrap.
#[cfg(test)]
mod cutover;
// cfg(test) for the same reason as support: its helpers unwrap.
#[cfg(test)]
mod commit_msg;
mod devlab_golden;
mod durations;
mod harness;
mod help;
mod install;
// cfg(test) for the same reason as support: its helpers unwrap.
#[cfg(test)]
mod lab;
// cfg(test) for the same reason as support: its helpers unwrap.
#[cfg(test)]
mod lab_guards;
// cfg(test) for the same reason as support: its helpers unwrap.
#[cfg(test)]
mod lab_events;
// cfg(test) for the same reason as support: its helpers unwrap.
#[cfg(test)]
mod lab_phones;
mod lab_ready;
// cfg(test) for the same reason as support: its helpers unwrap.
#[cfg(test)]
mod lab_reclaim;
// cfg(test) for the same reason as support: its helpers unwrap.
#[cfg(test)]
mod lab_share;
mod lead;
mod log;
// cfg(test) for the same reason as support: its helpers unwrap.
#[cfg(test)]
mod monitor;
// cfg(test) for the same reason as support: its helpers unwrap.
#[cfg(test)]
mod notify;
mod reserve;
mod router;
mod spawn;
mod wait_review;
// cfg(test) for the same reason as support: its helpers unwrap.
#[cfg(test)]
mod status;
// Marked cfg(test) so clippy treats these as test code and lets their helpers unwrap.
#[cfg(test)]
mod hook;
#[cfg(test)]
mod review;
#[cfg(test)]
mod support;
mod tiers;
