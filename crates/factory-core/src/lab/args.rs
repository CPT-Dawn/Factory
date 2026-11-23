//! The options devlab's `run` and `acquire` take, read as devlab read them.

use super::devices::Mode;

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

/// Options for run and acquire, the command after them, and the other `--flags` seen.
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
                let sec = number_arg(args.get(i));
                if !sec.is_finite() || sec < 0.0 {
                    return Err("--wait takes a number of seconds (0 = do not wait)".to_owned());
                }
                opts.wait_sec = Some(sec);
            }
            flag if flag.starts_with("--") => flags.push(flag.to_owned()),
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
    fn options_stop_at_the_command_and_keep_unknown_flags() {
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
        for bad in ["soon", "-1", "nan", "Infinity", "1e999"] {
            assert!(
                opts(&["--wait", bad])
                    .unwrap_err()
                    .starts_with("--wait takes"),
                "{bad}"
            );
        }
        assert_eq!(opts(&["--wait", " 7 "]).unwrap().0.wait_sec, Some(7.0));
    }
}
