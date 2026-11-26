//! The daemon's watch over phones (#55): a phone is prepped when adb's stream first shows it as
//! `device` and after every replug, every free phone is probed every `readinessCheckSec` (two
//! minutes unless set, #72; a leased one is its holder's and isn't), and its readiness is kept
//! in the store. Each change of it posts one desktop notification saying what to do: one per
//! change of a phone, told by its serial and named the same way each time, and an offline that its unplug soon follows is said as the unplug alone (#63). One
//! thread does it all, so the probes, the store's rows and the notifications follow the stream's
//! order; `devlab check` is answered on it too.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use factory_adapters::adb::{Adb, AdbCli};
use factory_adapters::desktop::{Desktop, NotifySend, PostError};
use factory_core::lab::config::MachineConfig;
use factory_core::lab::devices::{Attached, LOCK_PROBE, prep_command};
use factory_core::lab::events as event;
use factory_core::lab::ready::{
    AUTH_TIMEOUT_PROBE, BATTERY_PROBE, EpochMs, Known, Notice, POWER_PROBE, Probe, Readiness,
    STAY_ON_PROBE, after, auth_timeout_warning, judge, notice, phone_name, refresh_again,
    unplugged,
};

use super::super::check::PhoneCheck;
use super::super::readiness::{self, ReadyRow};
use super::super::{context, events, store};
use super::{Answer, Lab, Mark, failed, lock_ignoring_poison, now_ms};

/// How old a probe `devlab check` answers from may be.
const FRESH_MS: i64 = 10_000;
/// How long a grant waits for the watcher to bring the phones' readiness up to date.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `devlab check` waits for the watcher's answer.
const CHECK_TIMEOUT: Duration = Duration::from_secs(60);
/// How long an offline is held back: a phone being unplugged goes offline first, and its unplug
/// within this is said alone.
const FOLD: Duration = Duration::from_secs(3);

/// What the watcher thread is asked to do.
pub enum Job {
    /// adb's stream showed this list, and which attachment of each device it shows
    /// ([`super::DeviceList::since`]).
    Frame(Vec<Attached>, BTreeMap<String, i64>),
    /// adb's stream ended: whatever it shows next, the phones on it may have been plugged in
    /// again meanwhile.
    StreamEnded,
    /// `devlab check`: the phones named (every phone when none), answered on the sender; with
    /// `fresh`, each phone adb can use is probed now, whatever its last probe found.
    Check(Vec<String>, bool, mpsc::Sender<Vec<PhoneCheck>>),
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
                    lost: Vec::new(),
                    since: BTreeMap::new(),
                    said_missing: false,
                    doubted: HashSet::new(),
                    said: HashMap::new(),
                    models: HashMap::new(),
                    held: HashMap::new(),
                }
                .run(&rx);
            });
        if spawned.is_ok() && self.phones.set(tx).is_ok() {
            self.track();
        }
    }

    /// Tells the watcher what adb's stream showed; called before the list is told to anyone
    /// else, so anything that saw the list is answered after the watcher has it.
    pub(super) fn saw_phones(&self, list: &[Attached], since: &BTreeMap<String, i64>) {
        if let Some(tx) = self.phones.get() {
            let _ = tx.send(Job::Frame(list.to_vec(), since.clone()));
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
        self.ask_watcher(serials, false)
    }

    /// Every attached phone, each probed now: what `devlab status` shows of them (#57).
    pub(super) fn probe_phones(&self) -> Answer<Vec<PhoneCheck>> {
        self.ask_watcher(Vec::new(), true)
    }

    fn ask_watcher(&self, serials: Vec<String>, fresh: bool) -> Answer<Vec<PhoneCheck>> {
        let (reply, answer) = mpsc::channel();
        let tx = self
            .phones
            .get()
            .ok_or_else(|| failed(&"no phone watcher runs in this daemon"))?;
        tx.send(Job::Check(serials, fresh, reply))
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
    /// The phones the last list before adb's stream ended showed, until the next list comes.
    lost: Vec<Attached>,
    /// Which attachment of each device the last list shows.
    since: BTreeMap<String, i64>,
    /// Whether the log already said notify-send is missing.
    said_missing: bool,
    /// The phones whose last probe got no answer.
    doubted: HashSet<String>,
    /// What was last said of each phone, by serial: no change is said twice.
    said: HashMap<String, Said>,
    /// Each phone's model, by serial, as adb last named it: an offline phone's line names none.
    models: HashMap<String, String>,
    /// The offlines held back, by serial: each notice and when it is said.
    held: HashMap<String, (Notice, Instant)>,
}

/// What a notification said of a phone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Said {
    Is(Readiness),
    Unplugged,
}

/// The machine config now; its defaults when it can't be read (the CLI says why).
fn config() -> MachineConfig {
    context::machine_config().unwrap_or_default()
}

fn every(cfg: &MachineConfig) -> Duration {
    Duration::try_from_secs_f64(cfg.readiness_check_sec).unwrap_or(Duration::from_secs(120))
}

impl Watcher {
    fn run(&mut self, jobs: &mpsc::Receiver<Job>) {
        let mut next = Instant::now() + every(&config());
        loop {
            // What is due goes first, however many jobs wait: a stream of them that never lets
            // up holds back no re-check and no offline.
            self.say_held();
            if Instant::now() >= next {
                self.recheck();
                next = Instant::now() + every(&config());
            }
            let until = self
                .held
                .values()
                .map(|(_, at)| *at)
                .fold(next, Instant::min);
            match jobs.recv_timeout(until.saturating_duration_since(Instant::now())) {
                Ok(Job::Frame(list, since)) => {
                    self.since = since;
                    self.frame(list);
                    // Its probes are written: a request waiting for a phone looks again.
                    self.lab.notify();
                }
                // The next list's phones are all new to it: each free one is prepped and probed
                // again, and the ones it showed are kept to tell a leased phone that stayed.
                Ok(Job::StreamEnded) => {
                    let shown = std::mem::take(&mut self.shown);
                    if !shown.is_empty() {
                        self.lost = shown;
                    }
                    self.since.clear();
                }
                Ok(Job::Check(serials, fresh, reply)) => {
                    let _ = reply.send(self.check(&serials, fresh));
                }
                Ok(Job::Refresh(serials, by, reply)) => {
                    self.refresh(&serials, by);
                    let _ = reply.send(());
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    /// Probes each free phone adb can use. A leased phone is its holder's, whose run the
    /// watcher leaves alone: no prep or probe of its own touches the phone while a lease holds
    /// it (#72). Whether one does is read right before each probe, so a lease granted during
    /// the round is seen by the next phone's.
    fn recheck(&mut self) {
        for phone in self.shown.clone() {
            if phone.state == "device" && self.free(&phone.serial) {
                self.probe(&phone);
            }
        }
    }

    /// Whether no lease holds phone `serial`. A store that can't be read leaves it unknown,
    /// which is not free: the phone may be someone's.
    fn free(&self, serial: &str) -> bool {
        self.lab
            .read(store::taken)
            .is_ok_and(|taken| !taken.iter().any(|s| s == serial))
    }

    /// A new list: unplugged phones are forgotten, each phone that has just become `device`
    /// is prepped and probed, and one adb can't use is kept as adb says it is.
    fn frame(&mut self, list: Vec<Attached>) {
        let phones: Vec<Attached> = list.into_iter().filter(|a| a.kind == "phone").collect();
        let rows = self.lab.write(|c, _| readiness::all(c)).unwrap_or_default();
        // An offline phone's line names no model: the one adb, or its row, named before stays.
        for r in rows.iter().filter(|r| !r.model.is_empty()) {
            self.models
                .entry(r.serial.clone())
                .or_insert_with(|| r.model.clone());
        }
        for gone in rows
            .iter()
            .filter(|r| !phones.iter().any(|p| p.serial == r.serial))
        {
            let holder = self.lab.write(|c, _| {
                let holder = readiness::holder(c, &gone.serial)?;
                readiness::drop_row(c, &gone.serial)?;
                lock_ignoring_poison(&self.lab.marks).remove(&gone.serial);
                Ok(holder)
            });
            // Named as it was while attached: the list it was on is still the one shown.
            let name = self.name_of(&gone.serial);
            let said = unplugged(&name, holder.ok().flatten().as_deref());
            self.say(&gone.serial, Said::Unplugged, said);
        }
        for p in phones.iter().filter(|p| !p.model.is_empty()) {
            self.models.insert(p.serial.clone(), p.model.clone());
        }
        let before = std::mem::replace(&mut self.shown, phones.clone());
        let lost = std::mem::take(&mut self.lost);
        self.doubted.clear();
        for phone in &phones {
            let was = before.iter().find(|b| b.serial == phone.serial);
            if was.is_some_and(|b| b.state == phone.state) {
                continue;
            }
            if phone.state == "device" {
                // adb's stream came back and still shows it as it was: a free phone may have
                // been plugged in again meanwhile, and is readied again; a leased one is left
                // to its holder.
                let stayed = lost
                    .iter()
                    .any(|l| l.serial == phone.serial && l.state == "device");
                if stayed && !self.free(&phone.serial) {
                    continue;
                }
                let cfg = config();
                let _ = self.adb.shell(
                    &phone.serial,
                    &prep_command(cfg.prep_stay_awake, cfg.prep_animations_off),
                );
                self.probe(phone);
            } else {
                let mark = self.mark(&phone.serial);
                self.record(phone, judge(&unprobed(phone)), None, now_ms(), mark);
            }
        }
    }

    /// Of a probe of phone `serial` that begins now: its [`Mark`], of the attachment the last
    /// list shows.
    fn mark(&self, serial: &str) -> Option<Mark> {
        self.since.get(serial).map(|&attachment| Mark {
            attachment,
            at: Instant::now(),
        })
    }

    /// The name notifications give phone `serial`: its model, with the last 4 of its serial once
    /// another phone of the same model has been plugged in while this daemon runs, so that a
    /// phone keeps its name as its twin comes and goes; its serial while no model is known.
    fn name_of(&self, serial: &str) -> String {
        let model = self.models.get(serial).map_or("", String::as_str);
        let shared = !model.is_empty()
            && self
                .models
                .iter()
                .any(|(other, m)| other != serial && m == model);
        phone_name(model, serial, shared)
    }

    /// Says `n`: phone `serial` is now `what`, unless that was the last thing said of it. An
    /// offline is held back for [`FOLD`]: an unplug that follows is said in its place, and any
    /// other change has the offline said first.
    fn say(&mut self, serial: &str, what: Said, n: Notice) {
        if what == Said::Is(Readiness::Offline) {
            if self.said.get(serial) != Some(&what) && !self.held.contains_key(serial) {
                self.held
                    .insert(serial.to_owned(), (n, Instant::now() + FOLD));
            }
            return;
        }
        if let Some((offline, _)) = self.held.remove(serial)
            && what != Said::Unplugged
        {
            self.said
                .insert(serial.to_owned(), Said::Is(Readiness::Offline));
            self.post(&offline);
        }
        if self.said.get(serial) == Some(&what) {
            return;
        }
        self.said.insert(serial.to_owned(), what);
        self.post(&n);
    }

    /// Says each offline held back for [`FOLD`] that nothing followed.
    fn say_held(&mut self) {
        let now = Instant::now();
        let due: Vec<String> = self
            .held
            .iter()
            .filter(|(_, (_, at))| *at <= now)
            .map(|(serial, _)| serial.clone())
            .collect();
        for serial in due {
            if let Some((n, _)) = self.held.remove(&serial) {
                self.said.insert(serial, Said::Is(Readiness::Offline));
                self.post(&n);
            }
        }
    }

    /// Probes `phone` and records what it found.
    fn probe(&mut self, phone: &Attached) -> Option<ReadyRow> {
        // A probe is as old as its start: one begun before the phone was plugged in again is
        // not about it.
        let at = now_ms();
        let mark = self.mark(&phone.serial);
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
        self.record(phone, readiness, warning, at, mark)
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

    /// Keeps `phone`'s readiness, found by a probe begun at `at` (ms since the epoch, as
    /// shown) and `mark`, and says the notification a change of it earns.
    fn record(
        &mut self,
        phone: &Attached,
        readiness: Readiness,
        warning: Option<&str>,
        at: i64,
        mark: Option<Mark>,
    ) -> Option<ReadyRow> {
        let name = self.name_of(&phone.serial);
        let written = self.lab.write(|c, _| {
            let kept = readiness::get(c, &phone.serial)?;
            let before: Option<Known> = kept.as_ref().map(|r| r.known);
            // An offline phone's line names no model: its row keeps the one it had.
            let model = match kept {
                Some(r) if phone.model.is_empty() => r.model,
                _ => phone.model.clone(),
            };
            let row = ReadyRow {
                serial: phone.serial.clone(),
                model,
                known: after(before, readiness),
                warning: warning.map(str::to_owned),
                probed_ms: EpochMs(at),
            };
            readiness::put(c, &row)?;
            let mut marks = lock_ignoring_poison(&self.lab.marks);
            match mark {
                Some(mark) => marks.insert(phone.serial.clone(), mark),
                None => marks.remove(&phone.serial),
            };
            drop(marks);
            let said = notice(&name, before, readiness);
            let was = before.map(|k| k.readiness);
            let changed = was != Some(readiness);
            if changed {
                // Notified: the change earns a notification and notifications are on.
                let notified = said.is_some() && config().notify;
                let fields = event::readiness(&row.model, &phone.serial, was, readiness, notified);
                events::queue(c, "lab-readiness", &fields)?;
            }
            Ok((row, said, changed))
        });
        let (row, said, changed) = written.ok()?;
        if let Some(said) = said {
            self.say(&phone.serial, Said::Is(readiness), said);
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
            .kept(&phone.serial)
            .filter(|(r, age)| !stale(r.known.readiness, *age))
            .map(|(r, _)| r);
        match kept {
            Some(row) => Some(row),
            None if phone.state == "device" => self.probe(phone),
            None => {
                let mark = self.mark(&phone.serial);
                self.record(phone, judge(&unprobed(phone)), None, now_ms(), mark)
            }
        }
    }

    /// Phone `serial`'s row, and how many ms ago its probe began on the monotonic clock; as old
    /// as can be when that probe was of an earlier attachment of it, or of none known.
    fn kept(&self, serial: &str) -> Option<(ReadyRow, i64)> {
        let attachment = self.since.get(serial).copied();
        let (row, mark) = self
            .lab
            .write(|c, _| {
                let mark = lock_ignoring_poison(&self.lab.marks).get(serial).copied();
                Ok(readiness::get(c, serial)?.map(|row| (row, mark)))
            })
            .ok()
            .flatten()?;
        let age = mark
            .filter(|m| Some(m.attachment) == attachment)
            .map_or(i64::MAX, |m| m.age_ms(Instant::now()));
        Some((row, age))
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
            && self.kept(&phone.serial).is_some_and(|(r, age)| {
                r.known.readiness == Readiness::Ready && refresh_again(r.known.readiness, age)
            })
    }

    /// `devlab check`: each phone from a probe at most [`FRESH_MS`] old, probed again when
    /// it is older, or with `fresh` always.
    fn check(&mut self, serials: &[String], fresh: bool) -> Vec<PhoneCheck> {
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
                let stale: fn(Readiness, i64) -> bool = if fresh {
                    |_, _| true
                } else {
                    |_, age| age > FRESH_MS
                };
                match self.current(&phone, stale) {
                    Some(row) => PhoneCheck {
                        serial,
                        model: row.model,
                        readiness: Some(row.known.readiness),
                        warning: row.warning,
                        probed_ms: Some(row.probed_ms.0),
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
