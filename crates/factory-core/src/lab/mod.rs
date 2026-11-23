//! The lab's pure rules: devlab's stack-slot planner, kept apart from the store and the tools.
//!
//! - [`config`] reads the machine config and a project's `devlab.json`, and sets the per-owner cap.
//! - [`queue`] is the FIFO queue in which an owner at its cap steps aside.
//! - [`slots`] lays out a slot's ports, its Supabase config and its environment.
//! - [`grant`] decides, from one picture of the board, whether a request is served now.
//!
//! Devices (#16) and reservations (#17) get modules of their own beside these.

pub mod config;
pub mod grant;
pub mod queue;
pub mod slots;

/// `Math.round` as devlab's TypeScript did it: halves round up.
pub fn js_round(x: f64) -> i64 {
    (x + 0.5).floor() as i64
}

/// A number as JavaScript prints it, for the few numbers devlab's text echoes back.
pub fn js_number(x: f64) -> String {
    if x.fract() == 0.0 && x.abs() < 1e15 {
        format!("{x:.0}")
    } else {
        format!("{x}")
    }
}
