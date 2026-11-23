//! What Antigravity's own log says about a run: whether its terminal sandbox started.
//!
//! agy logs, klog style, to the file its `--log-file` names. With `--sandbox` in effect it logs
//! `Print mode: enabling terminal sandbox for this session` at startup (agy 1.5, established
//! by a real `agy -p` run, #29). Where the host can't run the sandbox it logs `Terminal sandbox
//! unavailable: <why>` and goes on without it, so `--sandbox` alone proves nothing.

/// What a run's log says about its sandbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sandbox {
    /// It started, and nothing says it was unavailable.
    Started,
    /// agy said it is unavailable, for this reason.
    Unavailable(String),
    /// The log never says it started.
    Unknown,
}

/// The line agy logs when its sandbox starts.
const STARTED: &str = "enabling terminal sandbox for this session";

/// What agy logs, in its own words, when the sandbox is unavailable, lowercased.
const UNAVAILABLE: [&str; 3] = [
    "terminal sandbox unavailable",
    "terminal sandbox is unavailable",
    "terminal sandbox is not available",
];

/// What `log`, agy's log of one run, says about its sandbox. A sign that it was unavailable
/// wins over one that it started.
pub fn sandbox(log: &str) -> Sandbox {
    let unavailable = log.lines().find(|line| {
        let line = line.to_lowercase();
        UNAVAILABLE.iter().any(|said| line.contains(said))
    });
    if let Some(line) = unavailable {
        // A klog line's message follows its header's `] `.
        let message = line.split_once("] ").map_or(line, |(_, message)| message);
        let lower = message.to_lowercase();
        let why = match lower.find("unavailable: ") {
            Some(at) => &message[at + "unavailable: ".len()..],
            None => message,
        };
        return Sandbox::Unavailable(why.trim().to_owned());
    }
    if log.lines().any(|line| line.contains(STARTED)) {
        Sandbox::Started
    } else {
        Sandbox::Unknown
    }
}
