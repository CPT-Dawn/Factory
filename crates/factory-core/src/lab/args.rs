//! The options devlab's `run` and `acquire` take, read as devlab read them.

use std::time::Duration;

use super::devices::Mode;

/// The longest a wait runs. Today's devlab waits a huge `--wait` out as good as forever; a
/// year keeps that meaning without overflowing a deadline.
pub const LONGEST_WAIT: Duration = Duration::from_secs(365 * 86400);

/// What a `--wait` that is no duration is refused with.
pub const WAIT_REFUSAL: &str =
    "--wait takes seconds, or a whole number and s, m or h such as 90s or 9m (0 = do not wait)";

/// Reads a `--wait`: seconds as devlab read them (see [`number_arg`]), or a whole number and
/// a unit as [`crate::duration::parse`] reads it, such as `90s` or `9m`. Refuses what is
/// neither, and a negative or endless number.
pub fn wait_arg(text: Option<&String>) -> Result<f64, String> {
    let refused = || WAIT_REFUSAL.to_owned();
    let sec = match text.map(|text| text.trim()) {
        Some(text) if text.ends_with(['s', 'm', 'h']) => crate::duration::parse(text)
            .map_err(|_| refused())?
            .as_secs_f64(),
        _ => number_arg(text),
    };
    if !sec.is_finite() || sec < 0.0 {
        return Err(refused());
    }
    Ok(sec)
}

/// What `acquire` and `run` ask for.
#[derive(Debug, Clone, PartialEq)]
pub struct Opts {
    /// Devices to hold; 0 holds only a stack slot.
    pub want: u32,
    /// Which kind of device: `--phone` or `--emulator`.
    pub mode: Mode,
    /// Start the slot's stack too.
    pub stack: bool,
    /// How long to queue; none takes the machine's default.
    pub wait_sec: Option<f64>,
}

impl Default for Opts {
    fn default() -> Opts {
        Opts {
            want: 1,
            mode: Mode::Any,
            stack: false,
            wait_sec: None,
        }
    }
}

/// Options for run and acquire, the command after them, and the other flags seen (`--quiet`);
/// any other `--flag` before the command or `--` is refused, naming it. A
/// `--wait -1` leaves `wait_sec` unset: the machine's default wait.
pub fn parse_opts(args: &[String]) -> Result<(Opts, &[String], Vec<String>), String> {
    let mut want = 1.0;
    let mut opts = Opts::default();
    let mut flags = Vec::new();
    let mut i = 0;
    while let Some(a) = args.get(i) {
        match a.as_str() {
            "--" => {
                i += 1;
                break;
            }
            "--devices" => {
                i += 1;
                want = number_arg(args.get(i));
            }
            "--pair" => want = 2.0,
            "--no-device" => want = 0.0,
            "--phone" => opts.mode = Mode::Phone,
            "--emulator" => opts.mode = Mode::Emulator,
            "--stack" => opts.stack = true,
            "--wait" => {
                i += 1;
                // -1 is the configured default, as it was for the old devlab; any other
                // negative is refused.
                opts.wait_sec = if number_arg(args.get(i)) == -1.0 {
                    None
                } else {
                    Some(wait_arg(args.get(i))?)
                };
            }
            "--quiet" => flags.push("--quiet".to_owned()),
            flag if flag.len() > 1 && flag.starts_with('-') => {
                return Err(format!("unknown option {flag}; devlab help"));
            }
            _ => break,
        }
        i += 1;
    }
    if want.fract() != 0.0 || !want.is_finite() || want < 0.0 {
        return Err("--devices takes a whole number".to_owned());
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        opts.want = want as u32;
    }
    Ok((opts, args.get(i..).unwrap_or_default(), flags))
}

/// How long a wait of `sec` seconds (finite and not negative, as [`parse_opts`] and the config
/// leave it) runs: its seconds, up to [`LONGEST_WAIT`].
pub fn wait_span(sec: f64) -> Duration {
    Duration::try_from_secs_f64(sec).map_or(LONGEST_WAIT, |wait| wait.min(LONGEST_WAIT))
}

/// An argument read as JavaScript's `Number(text)` reads it, near enough: blank is 0, junk (or
/// nothing) is NaN.
pub fn number_arg(text: Option<&String>) -> f64 {
    let Some(text) = text else { return f64::NAN };
    let text = text.trim();
    if text.is_empty() {
        return 0.0;
    }
    if text
        .chars()
        .any(|c| c.is_ascii_alphabetic() && !matches!(c, 'e' | 'E'))
    {
        return if text.trim_start_matches(['+', '-']) == "Infinity" {
            text.parse().unwrap_or(f64::NAN)
        } else {
            f64::NAN
        };
    }
    text.parse().unwrap_or(f64::NAN)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(args: &[&str]) -> Result<(Opts, Vec<String>, Vec<String>), String> {
        let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        parse_opts(&args).map(|(o, rest, flags)| (o, rest.to_vec(), flags))
    }

    #[test]
    fn options_stop_at_the_command_and_keep_quiet() {
        let (o, rest, flags) =
            opts(&["--no-device", "--stack", "--quiet", "--", "echo", "--x"]).unwrap();
        assert_eq!(
            (o.want, o.stack, rest, flags),
            (
                0,
                true,
                vec!["echo".into(), "--x".into()],
                vec!["--quiet".to_owned()]
            )
        );
        let (o, rest, _) = opts(&["--pair", "--wait", "2.5", "true"]).unwrap();
        assert_eq!(o.mode, Mode::Any);
        assert_eq!(opts(&["--phone"]).unwrap().0.mode, Mode::Phone);
        assert_eq!(opts(&["--emulator"]).unwrap().0.mode, Mode::Emulator);
        assert_eq!(
            (o.want, o.wait_sec, rest),
            (2, Some(2.5), vec!["true".to_owned()])
        );
    }

    #[test]
    fn an_unknown_flag_is_refused_by_name_and_dashes_still_end_the_options() {
        assert_eq!(
            opts(&["--abi", "x86"]).unwrap_err(),
            "unknown option --abi; devlab help"
        );
        assert_eq!(
            opts(&["--stack", "--phoen", "--", "true"]).unwrap_err(),
            "unknown option --phoen; devlab help"
        );
        assert_eq!(
            opts(&["-x", "--", "true"]).unwrap_err(),
            "unknown option -x; devlab help"
        );
        // After `--` or the command's first word, `--x` belongs to the command.
        let (_, rest, flags) = opts(&["--stack", "--", "--abi", "x86"]).unwrap();
        assert_eq!(
            (rest, flags),
            (vec!["--abi".to_owned(), "x86".to_owned()], vec![])
        );
        let (_, rest, _) = opts(&["true", "--abi"]).unwrap();
        assert_eq!(rest, vec!["true".to_owned(), "--abi".to_owned()]);
    }

    #[test]
    fn a_bad_count_or_wait_is_refused() {
        assert_eq!(
            opts(&["--devices", "two"]).unwrap_err(),
            "--devices takes a whole number"
        );
        assert_eq!(
            opts(&["--devices", "1.5"]).unwrap_err(),
            "--devices takes a whole number"
        );
        assert_eq!(
            opts(&["--devices"]).unwrap_err(),
            "--devices takes a whole number"
        );
        for bad in [
            "soon", "-2", "-0.5", "-1.5", "-1s", "-1m", "nan", "Infinity", "1e999",
        ] {
            assert!(opts(&["--wait", bad]).unwrap_err() == WAIT_REFUSAL, "{bad}");
        }
        assert_eq!(opts(&["--wait", " 7 "]).unwrap().0.wait_sec, Some(7.0));
    }

    #[test]
    fn a_wait_of_minus_one_asks_for_the_configured_default() {
        for text in ["-1", " -1 ", "-1.0", "-1e0"] {
            let (o, rest, _) = opts(&["--wait", text, "--no-device", "true"]).unwrap();
            assert_eq!(
                (o.wait_sec, rest),
                (None, vec!["true".to_owned()]),
                "{text:?}"
            );
        }
        // A later --wait still wins, and an earlier -1 does not stick.
        assert_eq!(
            opts(&["--wait", "-1", "--wait", "5"]).unwrap().0.wait_sec,
            Some(5.0)
        );
        assert_eq!(
            opts(&["--wait", "5", "--wait", "-1"]).unwrap().0.wait_sec,
            None
        );
    }

    #[test]
    fn a_wait_takes_the_duration_grammar_and_bare_numbers_as_devlab_read_them() {
        for (text, sec) in [
            ("9m", 540.0),
            ("90s", 90.0),
            ("1h", 3600.0),
            ("0s", 0.0),
            ("2.5", 2.5),
            (" 7 ", 7.0),
            ("1e3", 1000.0),
            ("", 0.0),
        ] {
            assert_eq!(
                opts(&["--wait", text]).unwrap().0.wait_sec,
                Some(sec),
                "{text:?}"
            );
        }
        for bad in ["9x", "1.5m", "m", "-1m", "9 m"] {
            assert_eq!(opts(&["--wait", bad]).unwrap_err(), WAIT_REFUSAL, "{bad:?}");
        }
    }

    #[test]
    fn a_wait_runs_its_seconds_up_to_a_year() {
        assert_eq!(wait_span(0.0), Duration::ZERO);
        assert_eq!(wait_span(2.5), Duration::from_millis(2500));
        assert_eq!(wait_span(31_536_000.0), Duration::from_secs(31_536_000));
        assert_eq!(wait_span(31_536_001.0), Duration::from_secs(31_536_000));
        assert_eq!(wait_span(1e19), Duration::from_secs(31_536_000));
    }
}
