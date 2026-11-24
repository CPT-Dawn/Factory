//! The daemon's watch over phones (#55): a phone is prepped when adb's stream first shows it as
//! `device` and after every replug, every attached phone is probed every `readinessCheckSec`,
//! and its readiness is kept in the store. Each change of it posts one desktop notification
//! saying what to do. One thread does it all, so the probes, the store's rows and the
//! notifications follow the stream's order; `devlab check` is answered on it too.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use factory_adapters::adb::{Adb, AdbCli};
use factory_adapters::desktop::{Desktop, NotifySend, PostError};
use factory_core::lab::config::MachineConfig;
use factory_core::lab::devices::{Attached, LOCK_PROBE, prep_command};
use factory_core::lab::ready::{
    AUTH_TIMEOUT_PROBE, BATTERY_PROBE, Notice, POWER_PROBE, Probe, Readiness, STAY_ON_PROBE, after,
    auth_timeout_warning, judge, notice, refresh_again, unplugged,
};

use super::super::check::PhoneCheck;
use super::super::context;
use super::super::readiness::{self, ReadyRow};
use super::{Answer, Lab, failed, now_ms};

/// How old a probe `devlab check` answers from may be.
const FRESH_MS: i64 = 10_000;
/// How long a grant waits for the watcher to bring the phones' readiness up to date.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `devlab check` waits for the watcher's answer.
const CHECK_TIMEOUT: Duration = Duration::from_secs(60);

/// What the watcher thread is asked to do.
pub enum Job {
    /// adb's stream showed this list.
    Frame(Vec<Attached>),
    /// adb's stream ended: whatever it shows next, the phones on it may have been plugged in
    /// again meanwhile.
    StreamEnded,
    /// `devlab check`: the phones named (every phone when none), answered on the sender.
    Check(Vec<String>, mpsc::Sender<Vec<PhoneCheck>>),
    /// A grant: bring these phones' readiness up to date, then answer on the sender.
    /// It is wanted by the instant given, and nothing is probed after it.
    Refresh(Vec<String>, Instant, mpsc::Sender<()>),
}

impl Lab {
    /// Starts the watcher thread, once, and the device tracker: phones are readied as they are
    /// plugged in, before any call needs the list.
    pub fn watch_phones(self: &Arc<Self>) {
        let (tx, rx) = mpsc::channel();
        let lab = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("lab-phones".to_owned())
            .spawn(move || {
                Watcher {
                    lab,
                    adb: Box::new(AdbCli),
                    desktop: Box::new(NotifySend),
                    shown: Vec::new(),
                    said_missing: false,
                    doubted: HashSet::new(),
                }
                .run(&rx);
            });
        if spawned.is_ok() && self.phones.set(tx).is_ok() {
            self.track();
        }
    }

    /// Tells the watcher what adb's stream showed; called before the list is told to anyone
    /// else, so anything that saw the list is answered after the watcher has it.
    pub(super) fn saw_phones(&self, list: &[Attached]) {
        if let Some(tx) = self.phones.get() {
            let _ = tx.send(Job::Frame(list.to_vec()));
        }
    }

    /// Tells the watcher adb's stream ended, before the next one shows a list.
    pub(super) fn phone_stream_ended(&self) {
        if let Some(tx) = self.phones.get() {
            let _ = tx.send(Job::StreamEnded);
        }
    }

    /// The phones `serials` names, every attached phone when none, as `devlab check` says them.
    pub(super) fn check_phones(&self, serials: Vec<String>) -> Answer<Vec<PhoneCheck>> {
        let (reply, answer) = mpsc::channel();
        let tx = self
            .phones
            .get()
            .ok_or_else(|| failed(&"no phone watcher runs in this daemon"))?;
        tx.send(Job::Check(serials, reply))
            .map_err(|_| failed(&"the phone watcher stopped"))?;
        answer
            .recv_timeout(CHECK_TIMEOUT)
            .map_err(|e| failed(&format!("the phone watcher did not answer: {e}")))
    }

    /// Has the watcher bring the readiness of the phones `serials` up to date for a grant: a
    /// probe that found the phone ready is kept while it is at most 8 s old ([`refresh_again`]),
    /// any other is made again. The grant then reads the store. It waits until `by` at most,
    /// and never more than 10 s: a watcher that is gone or slow leaves the rows as they are,
    /// which the grant trusts only while they are fresh.
    pub(super) fn refresh_phones(&self, serials: Vec<String>, by: Instant) {
        let (reply, answer) = mpsc::channel();
        let Some(tx) = self.phones.get() else {
            return;
        };
        let within = by.saturating_duration_since(Instant::now());
        if tx.send(Job::Refresh(serials, by, reply)).is_ok() {
            let _ = answer.recv_timeout(within.min(REFRESH_TIMEOUT));
        }
    }
}

struct Watcher {
    lab: Arc<Lab>,
    adb: Box<dyn Adb>,
    desktop: Box<dyn Desktop>,
    /// The phones the last list showed.
    shown: Vec<Attached>,
    /// Whether the log already said notify-send is missing.
    said_missing: bool,
    /// The phones whose last probe got no answer.
    doubted: HashSet<String>,
}

/// The machine config now; its defaults when it can't be read (the CLI says why).
fn config() -> MachineConfig {
    context::machine_config().unwrap_or_default()
}

fn every(cfg: &MachineConfig) -> Duration {
    Duration::try_from_secs_f64(cfg.readiness_check_sec).unwrap_or(Duration::from_secs(60))
}

/// The name a notification gives a phone: its model, else its serial.
fn name(model: &str, serial: &str) -> String {
    if model.is_empty() {
        serial.to_owned()
    } else {
        model.to_owned()
    }
}

impl Watcher {
    fn run(&mut self, jobs: &mpsc::Receiver<Job>) {
        let mut next = Instant::now() + every(&config());
        loop {
            match jobs.recv_timeout(next.saturating_duration_since(Instant::now())) {
                Ok(Job::Frame(list)) => {
                    self.frame(list);
                    // Its probes are written: a request waiting for a phone looks again.
                    self.lab.notify();
                }
                // The next list's phones are all new to it: each is prepped and probed again.
                Ok(Job::StreamEnded) => self.shown.clear(),
                Ok(Job::Check(serials, reply)) => {
                    let _ = reply.send(self.check(&serials));
                }
                Ok(Job::Refresh(serials, by, reply)) => {
                    self.refresh(&serials, by);
                    let _ = reply.send(());
                }
                Err(RecvTimeoutError::Timeout) => {
                    for phone in self.shown.clone() {
                        if phone.state == "device" {
                            self.probe(&phone);
                        }
                    }
                    next = Instant::now() + every(&config());
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    /// A new list: unplugged phones are forgotten, each phone that has just become `device`
    /// is prepped and probed, and one adb can't use is kept as adb says it is.
    fn frame(&mut self, list: Vec<Attached>) {
        let phones: Vec<Attached> = list.into_iter().filter(|a| a.kind == "phone").collect();
        let rows = self.lab.write(|c, _| readiness::all(c)).unwrap_or_default();
        for gone in rows
            .iter()
            .filter(|r| !phones.iter().any(|p| p.serial == r.serial))
        {
            let holder = self.lab.write(|c, _| {
                let holder = readiness::holder(c, &gone.serial)?;
                readiness::drop_row(c, &gone.serial)?;
                Ok(holder)
            });
            let name = name(&gone.model, &gone.serial);
            self.post(&unplugged(&name, holder.ok().flatten().as_deref()));
        }
        let before = std::mem::replace(&mut self.shown, phones.clone());
        self.doubted.clear();
        for phone in &phones {
            let was = before.iter().find(|b| b.serial == phone.serial);
            if was.is_some_and(|b| b.state == phone.state) {
                continue;
            }
            if phone.state == "device" {
                let cfg = config();
                let _ = self.adb.shell(
                    &phone.serial,
                    &prep_command(cfg.prep_stay_awake, cfg.prep_animations_off),
                );
                self.probe(phone);
            } else {
                self.record(phone, judge(&unprobed(phone)), None, now_ms());
            }
        }
    }

    /// Probes `phone` and records what it found.
    fn probe(&mut self, phone: &Attached) -> Option<ReadyRow> {
        // A probe is as old as its start: one begun before the phone was plugged in again is
        // not about it.
        let at = now_ms();
        let shell = |cmd: &str| self.adb.shell(&phone.serial, cmd);
        let probe = Probe {
            state: phone.state.clone(),
            stay_on: shell(STAY_ON_PROBE),
            battery: shell(BATTERY_PROBE),
            power: shell(POWER_PROBE),
            lock: shell(LOCK_PROBE),
        };
        let readiness = judge(&probe);
        // A phone that stops answering is most often one being unplugged, which the next list
        // tells: only a second probe in a row with no answer, of a phone adb still lists, finds
        // it offline.
        if readiness == Readiness::Offline
            && (!self.still_listed(&phone.serial) || self.doubted.insert(phone.serial.clone()))
        {
            return None;
        }
        self.doubted.remove(&phone.serial);
        let warning = if readiness == Readiness::Ready {
            auth_timeout_warning(shell(AUTH_TIMEOUT_PROBE).as_deref())
        } else {
            None
        };
        self.record(phone, readiness, warning, at)
    }

    /// Whether adb's latest list still shows `serial` as `device`.
    fn still_listed(&self, serial: &str) -> bool {
        self.lab
            .attached
            .borrow()
            .shown
            .as_ref()
            .is_some_and(|list| {
                list.iter()
                    .any(|a| a.serial == serial && a.state == "device")
            })
    }

    /// Keeps `phone`'s readiness, found at `at` (ms since the epoch), and posts the
    /// notification a change of it earns.
    fn record(
        &mut self,
        phone: &Attached,
        readiness: Readiness,
        warning: Option<&str>,
        at: i64,
    ) -> Option<ReadyRow> {
        let written = self.lab.write(|c, _| {
            let before = readiness::get(c, &phone.serial)?.map(|r| r.known);
            let row = ReadyRow {
                serial: phone.serial.clone(),
                model: phone.model.clone(),
                known: after(before, readiness),
                warning: warning.map(str::to_owned),
                probed_ms: at,
            };
            readiness::put(c, &row)?;
            Ok((
                row,
                notice(&name(&phone.model, &phone.serial), before, readiness),
                before.is_none_or(|k| k.readiness != readiness),
            ))
        });
        let (row, said, changed) = written.ok()?;
        if let Some(said) = said {
            self.post(&said);
        }
        if changed {
            // A request waiting for a phone looks again.
            self.lab.notify();
        }
        Some(row)
    }

    /// Logs `n`, and posts it when the machine config has notifications on.
    fn post(&mut self, n: &Notice) {
        let body = if n.body.is_empty() {
            String::new()
        } else {
            format!(" - {}", n.body)
        };
        eprintln!("factory: lab: {}{body}", n.summary);
        if !config().notify {
            return;
        }
        match self
            .desktop
            .post("devlab", n.urgency.word(), &n.summary, &n.body)
        {
            Ok(()) => {}
            Err(PostError::Missing) => {
                if !self.said_missing {
                    eprintln!(
                        "factory: lab: notify-send not found: no desktop notifications until it is installed"
                    );
                    self.said_missing = true;
                }
            }
            Err(PostError::Failed(e)) => eprintln!("factory: lab: {e}"),
        }
    }

    /// `phone`'s row: the kept one unless `stale` says, of its readiness and its age in ms, that
    /// it is, else what a new probe finds. None: a phone that did not answer.
    fn current(&mut self, phone: &Attached, stale: fn(Readiness, i64) -> bool) -> Option<ReadyRow> {
        let kept = self
            .lab
            .write(|c, _| readiness::get(c, &phone.serial))
            .ok()
            .flatten()
            .filter(|r| !stale(r.known.readiness, now_ms() - r.probed_ms));
        match kept {
            Some(row) => Some(row),
            None if phone.state == "device" => self.probe(phone),
            None => self.record(phone, judge(&unprobed(phone)), None, now_ms()),
        }
    }

    /// Brings the rows of `serials` up to date for a grant that reads them right after, by
    /// `by` and within [REFRESH_TIMEOUT]: no probe starts after.
    fn refresh(&mut self, serials: &[String], by: Instant) {
        let by = by.min(Instant::now() + REFRESH_TIMEOUT);
        let phones: Vec<Attached> = serials
            .iter()
            .filter_map(|s| self.shown.iter().find(|p| &p.serial == s).cloned())
            .collect();
        for phone in &phones {
            if Instant::now() >= by {
                return;
            }
            self.current(phone, refresh_again);
        }
        // Probing one phone can take long enough for another's ready probe to come due: it is
        // made again too, while there is time.
        loop {
            let mut probed = false;
            for phone in &phones {
                if Instant::now() >= by {
                    return;
                }
                if self.lapsing(phone) {
                    self.probe(phone);
                    probed = true;
                }
            }
            if !probed {
                return;
            }
        }
    }

    /// Whether `phone`, as adb can use it, has a ready probe a grant will soon find too old.
    fn lapsing(&self, phone: &Attached) -> bool {
        phone.state == "device"
            && self
                .lab
                .write(|c, _| readiness::get(c, &phone.serial))
                .ok()
                .flatten()
                .is_some_and(|r| {
                    r.known.readiness == Readiness::Ready
                        && refresh_again(r.known.readiness, now_ms() - r.probed_ms)
                })
    }

    /// `devlab check`: each phone from a probe at most [`FRESH_MS`] old, probed again when
    /// it is older.
    fn check(&mut self, serials: &[String]) -> Vec<PhoneCheck> {
        let wanted: Vec<String> = if serials.is_empty() {
            self.shown.iter().map(|p| p.serial.clone()).collect()
        } else {
            serials.to_vec()
        };
        wanted
            .into_iter()
            .map(|serial| {
                let Some(phone) = self.shown.iter().find(|p| p.serial == serial).cloned() else {
                    return PhoneCheck {
                        serial,
                        model: String::new(),
                        readiness: None,
                        warning: None,
                        probed_ms: None,
                    };
                };
                match self.current(&phone, |_, age| age > FRESH_MS) {
                    Some(row) => PhoneCheck {
                        serial,
                        model: row.model,
                        readiness: Some(row.known.readiness),
                        warning: row.warning,
                        probed_ms: Some(row.probed_ms),
                    },
                    None => PhoneCheck {
                        serial,
                        model: phone.model,
                        readiness: Some(Readiness::Offline),
                        warning: None,
                        probed_ms: None,
                    },
                }
            })
            .collect()
    }
}

/// A phone adb lists but can't use: its state alone says what it is.
fn unprobed(phone: &Attached) -> Probe {
    Probe {
        state: phone.state.clone(),
        stay_on: None,
        battery: None,
        power: None,
        lock: None,
    }
}
