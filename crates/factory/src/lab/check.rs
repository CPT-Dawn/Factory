//! `devlab check [serial...]` (#55): one line per phone, ready or what is wrong and what to do;
//! exit 0 when every phone it names is ready, 10 when any needs attention. The daemon answers
//! from its last probe, probing again one older than 10 s.

use factory_core::exit::Exit;
use factory_core::lab::ready::{Readiness, check_line};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::client;
use crate::output::Failure;
use crate::paths::Paths;

/// The readiness of phones: [`CheckParams`] in, a [`PhoneCheck`] per phone out.
pub const CHECK: &str = "lab.check";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckParams {
    /// The phones to check; none: every phone adb lists.
    pub serials: Vec<String>,
}

/// One phone as `devlab check` sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PhoneCheck {
    pub serial: String,
    pub model: String,
    /// Its readiness word; none when adb doesn't list it.
    pub readiness: Option<String>,
    /// What a ready phone is warned of.
    pub warning: Option<String>,
    /// When it was probed, in ms since the epoch.
    pub probed_ms: Option<i64>,
}

impl PhoneCheck {
    pub fn ready(&self) -> bool {
        self.readiness.as_deref() == Some(Readiness::Ready.word())
    }

    pub fn line(&self) -> String {
        check_line(
            &self.serial,
            &self.model,
            self.readiness.as_deref().and_then(Readiness::from_word),
            self.warning.as_deref(),
        )
    }
}

/// What `check` says when adb lists no phone.
pub const NO_PHONE: &str = "no phone attached: plug one in and allow USB debugging";

/// Asks the daemon about the phones `args` name (every phone when none); `--json` is the
/// only flag.
pub fn phones(args: &[String]) -> Result<Vec<PhoneCheck>, Failure> {
    let mut serials = Vec::new();
    for arg in args {
        if arg == "--json" {
            continue;
        }
        if arg.starts_with('-') {
            return Err(Failure::usage(format!(
                "unknown option {arg}: devlab check [serial...] [--json]"
            )));
        }
        serials.push(arg.clone());
    }
    client::call(&Paths::from_env()?, CHECK, &CheckParams { serials })
}

/// The exit code: 0 when there are phones and every one is ready.
pub fn code(phones: &[PhoneCheck]) -> u8 {
    if !phones.is_empty() && phones.iter().all(PhoneCheck::ready) {
        Exit::Ok.code()
    } else {
        Exit::NeedsAttention.code()
    }
}

/// The lines `check` prints: one per phone.
pub fn lines(phones: &[PhoneCheck]) -> Vec<String> {
    if phones.is_empty() {
        return vec![NO_PHONE.to_owned()];
    }
    phones.iter().map(PhoneCheck::line).collect()
}

pub fn to_json(phones: &[PhoneCheck]) -> Value {
    let each: Vec<Value> = phones
        .iter()
        .map(|p| {
            let mut v = serde_json::to_value(p).unwrap_or(Value::Null);
            v["line"] = json!(p.line());
            v["ready"] = json!(p.ready());
            v
        })
        .collect();
    json!({ "phones": each, "ready": code(phones) == 0 })
}
