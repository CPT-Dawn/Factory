//! The lab's pure rules: devlab's stack-slot planner, kept apart from the store and the tools.
//!
//! - [`args`] reads the options of devlab's `run` and `acquire`.
//! - [`config`] reads the machine config and a project's `devlab.json`, and sets the per-owner cap.
//! - [`queue`] is the FIFO queue in which an owner at its cap steps aside.
//! - [`slots`] lays out a slot's ports, its Supabase config and its environment.
//! - [`grant`] decides, from one picture of the board, whether a request is served now.
//! - [`run`] says how a `devlab run` ended.
//!
//! - [`reserve`] sets slots aside for a lead's implementers.
//!
//! Devices (#16) get a module of their own beside these.

pub mod args;
pub mod config;
pub mod grant;
pub mod queue;
pub mod reserve;
pub mod run;
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

/// How long ago `then` was, as devlab's status says it: `3m`, or `1.5h` from an hour on.
pub fn ago(now: i64, then: i64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let m = js_round((now - then) as f64 / 60.0);
    if m < 60 {
        format!("{m}m")
    } else {
        #[allow(clippy::cast_precision_loss)]
        let tenths = js_round(m as f64 / 6.0);
        #[allow(clippy::cast_precision_loss)]
        let hours = tenths as f64 / 10.0;
        format!("{}h", js_number(hours))
    }
}

#[cfg(test)]
mod tests {
    use super::ago;

    #[test]
    fn ages_read_in_minutes_then_tenths_of_hours() {
        assert_eq!(ago(1000, 1000), "0m");
        assert_eq!(ago(1000 + 89, 1000), "1m");
        assert_eq!(ago(3600 * 2 + 1000, 1000), "2h");
        assert_eq!(ago(5400 + 1000, 1000), "1.5h");
    }
}
