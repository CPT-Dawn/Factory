//! The context steward: the daemon's watch over every lead's and subagent's context.
//!
//! `factory monitor start` switches it on and `stop` off; the switch is kept in the database, so
//! a restarted daemon resumes it. While on, one task follows herdr's picture of the leads and
//! inotify's word on their session logs, and nothing polls: a lead's context is the latest
//! reading its log gives, a subagent's likewise. The rules are `factory_core::steward`'s.
//!
//! The context table keeps each session's latest context, and whether its lead was asked or
//! told, so a restart asks and tells nobody twice.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use factory_adapters::herdr::{Herdr, HerdrApi, PromptWait};
use factory_core::lead::AgentStatus;
use factory_core::steward::{LeadNow, Policy, Step};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use crate::config;
use crate::leads::{Leads, Seen};
use crate::monitor::kilo;
use crate::output::Failure;
use crate::paths::Paths;
use crate::protocol::{LeadContext, MonitorStatus, MonitorSwitch, SubagentContext};
use crate::sessions::{self, Change, Inotify, SubagentFile, Tail};
use crate::store::{Context, Store};

/// How long the steward waits before it tries again what herdr refused or couldn't do.
const RETRY: Duration = Duration::from_secs(30);
/// How long a compaction may take, as ctx-monitor.sh allowed it.
const COMPACT_TIMEOUT: Duration = Duration::from_secs(600);
const COMPACT: &str = "/compact Keep: the task and its limits from the first prompt, the report \
     path, the integration branch and PR, each ticket's status, open review findings, decisions \
     made so far, and the next step.";
const CONTINUE: &str = "Compacted. Continue where you left off.";

pub struct Steward<H: HerdrApi = Herdr> {
    leads: Arc<Leads<H>>,
    paths: Paths,
    store: Mutex<Store>,
    /// Held while the steward is switched, so one switch runs at a time.
    turn: tokio::sync::Mutex<()>,
    /// The running steward's task; none while it is off.
    task: Mutex<Option<AbortHandle>>,
    /// The context table as the task last saw it.
    table: Mutex<Vec<LeadContext>>,
}

impl<H: HerdrApi> Steward<H> {
    pub fn new(paths: Paths, leads: Arc<Leads<H>>, store: Store) -> Arc<Steward<H>> {
        Arc::new(Steward {
            leads,
            paths,
            store: Mutex::new(store),
            turn: tokio::sync::Mutex::new(()),
            task: Mutex::new(None),
            table: Mutex::new(Vec::new()),
        })
    }

    /// At the daemon's start: resumes a steward left running, trying again while it can't.
    pub async fn resume(self: Arc<Self>) {
        loop {
            let running = lock(&self.store).monitor_running();
            match running {
                Ok(true) => {}
                Ok(false) => return,
                Err(e) => {
                    eprintln!("factory: monitor: cannot read its switch from the database: {e}");
                    return;
                }
            }
            match self.start().await {
                Ok(_) => return,
                Err(failure) => eprintln!(
                    "factory: monitor: cannot resume: {}; trying again in {}s",
                    failure.message,
                    RETRY.as_secs()
                ),
            }
            tokio::time::sleep(RETRY).await;
        }
    }

    pub async fn start(self: &Arc<Self>) -> Result<MonitorSwitch, Failure> {
        let _turn = self.turn.lock().await;
        if self.running() {
            return Ok(switched(true, false));
        }
        let policy = config::monitor_policy(&self.paths)?;
        self.leads.check().await?;
        let inotify = Inotify::new().map_err(|e| {
            Failure::precondition(format!(
                "cannot watch session logs: {e}; raise fs.inotify.max_user_instances, then retry"
            ))
        })?;
        self.remember(true)?;
        let task = tokio::spawn(Arc::clone(self).run(policy, inotify));
        *lock(&self.task) = Some(task.abort_handle());
        Ok(switched(true, true))
    }

    /// Stops the steward; a compaction under way still finishes.
    pub async fn stop(&self) -> Result<MonitorSwitch, Failure> {
        let _turn = self.turn.lock().await;
        let task = lock(&self.task).take();
        let was = task.as_ref().is_some_and(|task| !task.is_finished());
        if let Some(task) = task {
            task.abort();
            lock(&self.table).clear();
            // A task that ended on its own left the switch on; it goes off here too.
            self.remember(false)?;
        }
        Ok(switched(false, was))
    }

    /// Whether the steward's task runs: one that ended on its own, or panicked, does not.
    fn running(&self) -> bool {
        lock(&self.task)
            .as_ref()
            .is_some_and(|task| !task.is_finished())
    }

    pub fn status(&self) -> MonitorStatus {
        let running = self.running();
        MonitorStatus {
            running,
            leads: if running {
                lock(&self.table).clone()
            } else {
                Vec::new()
            },
        }
    }

    fn remember(&self, running: bool) -> Result<(), Failure> {
        lock(&self.store)
            .set_monitor_running(running)
            .map_err(|e| {
                Failure::precondition(format!(
                    "cannot record the monitor's switch in the database: {e}; check the disk, then retry"
                ))
            })
    }

    /// The steward's task: looks at every lead after each change, until it is stopped.
    async fn run(self: Arc<Self>, policy: Policy, mut inotify: Inotify) {
        let (done, mut compacted) = mpsc::unbounded_channel();
        let mut changes = self.leads.changes();
        let mut state = State {
            policy,
            projects: self.paths.claude_projects(),
            leads: BTreeMap::new(),
            dirs: HashMap::new(),
            down: false,
        };
        loop {
            changes.borrow_and_update();
            let now = Instant::now();
            let wake = match self.leads.seen() {
                Some(seen) => {
                    state.down = false;
                    self.follow(&mut state, &inotify, &seen, now);
                    self.act(&mut state, &done, now).await
                }
                None => match self.leads.check().await {
                    Ok(()) => continue,
                    Err(failure) => {
                        if !state.down {
                            eprintln!("factory: monitor: {}", failure.message);
                        }
                        state.down = true;
                        Some(now + RETRY)
                    }
                },
            };
            self.publish(&state);
            let woke = tokio::select! {
                changed = changes.changed() => if changed.is_ok() { Woke::Leads } else { Woke::Stop },
                files = inotify.next() => Woke::Files(files),
                Some(compaction) = compacted.recv() => Woke::Compacted(compaction),
                () = sleep_until(wake) => Woke::Timer,
            };
            match woke {
                Woke::Leads | Woke::Timer => {}
                Woke::Stop => return,
                Woke::Files(Ok(files)) => self.changed(&mut state, &inotify, files),
                Woke::Files(Err(e)) => {
                    eprintln!(
                        "factory: monitor: lost inotify: {e}; stopped, run factory monitor start to start it again"
                    );
                    return;
                }
                Woke::Compacted(compaction) => self.compacted(&mut state, compaction),
            }
        }
    }

    /// Brings the watched leads in line with herdr's picture: a lead that started a session
    /// starts afresh, one that finished or went is dropped.
    fn follow(&self, state: &mut State, inotify: &Inotify, seen: &[Seen], now: Instant) {
        let live: Vec<&Seen> = seen
            .iter()
            .filter(|lead| lead.agent.is_some() && !lead.reported)
            .collect();
        state
            .leads
            .retain(|name, _| live.iter().any(|lead| lead.name == *name));
        for lead in live {
            let log = lead.session.as_ref().and_then(|session| {
                sessions::lead_log(&state.projects, lead.cwd.as_deref(), session)
            });
            let watched = state
                .leads
                .entry(lead.name.clone())
                .or_insert_with(|| Watched::new(None));
            if watched.log != log {
                *watched = Watched::new(log);
            }
            watched.session = lead.session.as_ref().map(|session| session.value.clone());
            let idle = matches!(lead.agent, Some(AgentStatus::Idle | AgentStatus::Done));
            watched.waiting = lead.waiting;
            // A lead that worked since it went idle, even unseen between two looks, is idle anew.
            let moved = lead.since.into_std();
            watched.idle_since = if idle && !lead.waiting {
                Some(watched.idle_since.map_or(now, |since| since.max(moved)))
            } else {
                None
            };
            if watched.tail.is_none() {
                self.open_lead(&lead.name, watched, now);
            }
        }
        self.watch_dirs(state, inotify);
    }

    /// Opens a lead's log once it exists, taking its latest context and what the table says
    /// the steward did.
    fn open_lead(&self, name: &str, watched: &mut Watched, now: Instant) {
        let Some(log) = watched.log.clone().filter(|log| log.is_file()) else {
            return;
        };
        match Tail::open(&log, false) {
            Ok((tail, reading)) => {
                watched.tail = Some(tail);
                let row = self.row(&log);
                if row.as_ref().is_some_and(|row| row.asked) {
                    watched.asked_at = Some(now);
                }
                watched.tokens = reading.map(|r| r.tokens);
                self.save_lead(name, watched);
            }
            Err(e) => unreadable(&log, &e),
        }
    }

    /// Watches the directories the leads' logs and their subagents' logs are in, and no others;
    /// a directory watched anew is read for the subagents already in it.
    fn watch_dirs(&self, state: &mut State, inotify: &Inotify) {
        let mut wanted: BTreeSet<PathBuf> = BTreeSet::new();
        let names: Vec<String> = state.leads.keys().cloned().collect();
        for name in names {
            let Some(log) = state.leads.get(&name).and_then(|lead| lead.log.clone()) else {
                continue;
            };
            let session = sessions::session_dir(&log);
            let subagents = session.join("subagents");
            let dirs = [
                (log.parent().map(Path::to_path_buf), Role::Project),
                (Some(session), Role::Session),
                (Some(subagents.clone()), Role::Subagents(name.clone())),
            ];
            // Parent first: a directory made after its parent is watched is heard of.
            for (dir, role) in dirs {
                let Some(dir) = dir else { continue };
                wanted.insert(dir.clone());
                if state.dirs.values().any(|watch| watch.path == dir) || !dir.is_dir() {
                    continue;
                }
                match inotify.watch(&dir) {
                    Ok(wd) => {
                        state.dirs.insert(
                            wd,
                            Dir {
                                path: dir.clone(),
                                role,
                            },
                        );
                    }
                    Err(e) => {
                        eprintln!(
                            "factory: monitor: cannot watch {}: {e}; raise \
                             fs.inotify.max_user_watches if it is out of watches, then run \
                             factory monitor stop and start",
                            dir.display()
                        );
                        continue;
                    }
                }
                if dir == subagents {
                    self.scan(state, &name, &subagents);
                }
            }
        }
        state.dirs.retain(|wd, dir| {
            let keep = wanted.contains(&dir.path);
            if !keep {
                inotify.unwatch(*wd);
            }
            keep
        });
    }

    /// Takes in every subagent whose log is in `dir`.
    fn scan(&self, state: &mut State, lead: &str, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            if let Some(SubagentFile::Log(agent)) = sessions::subagent_file(&entry.file_name()) {
                self.subagent(state, lead, &agent, &entry.path());
            }
        }
    }

    /// Reads a subagent's new lines, taking it in first if it is new.
    fn subagent(&self, state: &mut State, lead: &str, agent: &str, log: &Path) {
        let Some(watched) = state.leads.get_mut(lead) else {
            return;
        };
        if watched.finished.contains(agent) {
            return;
        }
        if let Some(sub) = watched.subagents.get_mut(agent) {
            match sub.tail.read() {
                Ok(Some(reading)) => {
                    sub.tokens = Some(reading.tokens);
                    self.save_subagent(lead, agent, sub);
                }
                Ok(None) => {}
                Err(e) => unreadable(log, &e),
            }
            return;
        }
        let meta = sessions::meta(log);
        if meta.finished {
            watched.finished.insert(agent.to_owned());
            return;
        }
        match Tail::open(log, true) {
            Ok((tail, reading)) => {
                let told = self.row(log).is_some_and(|row| row.told);
                let sub = Sub {
                    tail,
                    description: meta.description,
                    tokens: reading.map(|r| r.tokens),
                    told,
                    retry_at: None,
                };
                self.save_subagent(lead, agent, &sub);
                watched.subagents.insert(agent.to_owned(), sub);
            }
            Err(e) => unreadable(log, &e),
        }
    }

    /// Takes in what inotify reported.
    fn changed(&self, state: &mut State, inotify: &Inotify, changes: Vec<Change>) {
        let mut relist = false;
        for change in changes {
            if change.overflow {
                self.reread(state, inotify);
                continue;
            }
            if change.gone {
                state.dirs.remove(&change.watch);
                continue;
            }
            let (Some(dir), Some(name)) = (state.dirs.get(&change.watch), change.name) else {
                continue;
            };
            let path = dir.path.join(&name);
            match dir.role.clone() {
                Role::Project => {
                    let mut known = false;
                    for (lead, watched) in &mut state.leads {
                        if watched.log.as_ref() != Some(&path) {
                            continue;
                        }
                        known = true;
                        self.read_lead(lead, watched);
                    }
                    let log = path.extension().is_some_and(|ext| ext == "jsonl");
                    // A lead may have started a session herdr hasn't said yet.
                    relist |= change.appeared && log && !known;
                    if path.is_dir() {
                        self.watch_dirs(state, inotify);
                    }
                }
                Role::Session => {
                    if name == "subagents" {
                        self.watch_dirs(state, inotify);
                    }
                }
                Role::Subagents(lead) => match sessions::subagent_file(&name) {
                    Some(SubagentFile::Log(agent)) => self.subagent(state, &lead, &agent, &path),
                    Some(SubagentFile::Meta(agent)) => {
                        self.meta_changed(state, &lead, &agent, &path);
                    }
                    None => {}
                },
            }
        }
        if relist {
            self.leads.relist();
        }
    }

    /// A subagent's meta file changed: it may say what the subagent is for, or that it finished.
    fn meta_changed(&self, state: &mut State, lead: &str, agent: &str, meta_file: &Path) {
        let Some(watched) = state.leads.get_mut(lead) else {
            return;
        };
        let log = meta_file.with_file_name(format!("{agent}.jsonl"));
        let meta = sessions::meta(&log);
        if meta.finished {
            watched.subagents.remove(agent);
            watched.finished.insert(agent.to_owned());
        } else if let Some(sub) = watched.subagents.get_mut(agent) {
            sub.description = meta.description.or(sub.description.take());
        }
    }

    /// Reads every log again after inotify lost events.
    fn reread(&self, state: &mut State, inotify: &Inotify) {
        for (lead, watched) in &mut state.leads {
            self.read_lead(lead, watched);
        }
        let dirs: Vec<(String, PathBuf)> = state
            .dirs
            .values()
            .filter_map(|dir| match &dir.role {
                Role::Subagents(lead) => Some((lead.clone(), dir.path.clone())),
                _ => None,
            })
            .collect();
        for (lead, dir) in dirs {
            self.scan(state, &lead, &dir);
        }
        self.watch_dirs(state, inotify);
    }

    /// Reads a lead's new lines, opening its log first if it isn't yet.
    fn read_lead(&self, name: &str, watched: &mut Watched) {
        let Some(tail) = watched.tail.as_mut() else {
            self.open_lead(name, watched, Instant::now());
            return;
        };
        match tail.read() {
            Ok(Some(reading)) => {
                watched.tokens = Some(reading.tokens);
                self.save_lead(name, watched);
            }
            Ok(None) => {}
            Err(e) => unreadable(tail.path(), &e),
        }
    }

    /// Takes each lead's and subagent's next step; returns when to look again, if a step waits.
    async fn act(
        &self,
        state: &mut State,
        done: &mpsc::UnboundedSender<Compacted>,
        now: Instant,
    ) -> Option<Instant> {
        let mut wake: Option<Instant> = None;
        let mut at = |instant: Instant| wake = Some(wake.map_or(instant, |w| w.min(instant)));
        let policy = state.policy;
        for (name, lead) in &mut state.leads {
            for (agent, sub) in &mut lead.subagents {
                let Some(tokens) = sub.tokens else { continue };
                if !policy.tells(tokens, sub.told) {
                    continue;
                }
                if let Some(retry) = sub.retry_at.filter(|retry| now < *retry) {
                    at(retry);
                    continue;
                }
                let text = tell(
                    agent,
                    sub.description.as_deref(),
                    tokens,
                    policy.subagent_at,
                );
                match self.leads.prompt(name, &text, None).await {
                    Ok(()) => {
                        sub.told = true;
                        sub.retry_at = None;
                        self.save_subagent(name, agent, sub);
                    }
                    Err(e) => {
                        eprintln!(
                            "factory: monitor: cannot tell {name} about {agent}: {e}; trying again in {}s",
                            RETRY.as_secs()
                        );
                        sub.retry_at = Some(now + RETRY);
                        at(now + RETRY);
                    }
                }
            }
            if lead.compacting {
                continue;
            }
            if let Some(retry) = lead.retry_at.filter(|retry| now < *retry) {
                at(retry);
                continue;
            }
            let facts = LeadNow {
                tokens: lead.tokens,
                idle_since: lead.idle_since,
                waiting: lead.waiting,
                asked_at: lead.asked_at,
            };
            match policy.step(&facts, now) {
                Step::Nothing => {}
                Step::At(instant) => at(instant),
                Step::Unask => {
                    lead.asked_at = None;
                    self.save_lead(name, lead);
                }
                // Prompts to the leads before took time: a wait registered meanwhile still holds.
                Step::Ask | Step::Compact if self.leads.waiting(name) => {
                    lead.waiting = true;
                    lead.idle_since = None;
                }
                Step::Ask => {
                    let text = ask(lead.tokens.unwrap_or_default());
                    match self.leads.prompt(name, &text, None).await {
                        Ok(()) => {
                            lead.asked_at = Some(now);
                            lead.retry_at = None;
                            self.save_lead(name, lead);
                            if let Some(idle) = lead.idle_since {
                                at(idle.max(now) + policy.grace);
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "factory: monitor: cannot ask {name} to stop: {e}; trying again in {}s",
                                RETRY.as_secs()
                            );
                            lead.retry_at = Some(now + RETRY);
                            at(now + RETRY);
                        }
                    }
                }
                Step::Compact => {
                    lead.compacting = true;
                    let leads = Arc::clone(&self.leads);
                    let compaction = Compacted {
                        lead: name.clone(),
                        log: lead.log.clone(),
                        outcome: Outcome::Failed,
                    };
                    tokio::spawn(compact(leads, compaction, done.clone()));
                }
            }
        }
        wake
    }

    /// A compaction ended: the lead's context is unknown until its log says again.
    fn compacted(&self, state: &mut State, compaction: Compacted) {
        let Some(lead) = state.leads.get_mut(&compaction.lead) else {
            return;
        };
        if lead.log != compaction.log {
            return;
        }
        lead.compacting = false;
        match compaction.outcome {
            Outcome::Done => {
                lead.tokens = None;
                lead.asked_at = None;
                self.save_lead(&compaction.lead, lead);
            }
            Outcome::Failed => lead.retry_at = Some(Instant::now() + RETRY),
            Outcome::Waiting => {}
        }
    }

    fn publish(&self, state: &State) {
        let table = state
            .leads
            .iter()
            .map(|(name, lead)| LeadContext {
                lead: name.clone(),
                session: lead.session.clone(),
                log: lead
                    .log
                    .as_ref()
                    .map(|log| log.to_string_lossy().into_owned()),
                tokens: lead.tokens,
                asked: lead.asked_at.is_some(),
                compacting: lead.compacting,
                subagents: lead
                    .subagents
                    .iter()
                    .map(|(agent, sub)| SubagentContext {
                        agent: agent.clone(),
                        description: sub.description.clone(),
                        tokens: sub.tokens,
                        told: sub.told,
                    })
                    .collect(),
            })
            .collect();
        *lock(&self.table) = table;
    }

    fn row(&self, log: &Path) -> Option<Context> {
        lock(&self.store)
            .context(&log.to_string_lossy())
            .unwrap_or_else(|e| {
                eprintln!("factory: monitor: cannot read the context table: {e}; check the disk, then run factory monitor stop and start");
                None
            })
    }

    fn save(&self, context: &Context) {
        if let Err(e) = lock(&self.store).set_context(context) {
            eprintln!(
                "factory: monitor: cannot write the context table: {e}; check the disk, else a restart may ask or tell again"
            );
        }
    }

    fn save_lead(&self, name: &str, lead: &Watched) {
        let Some(log) = &lead.log else { return };
        self.save(&Context {
            log: log.to_string_lossy().into_owned(),
            lead: name.to_owned(),
            agent: None,
            tokens: lead.tokens,
            asked: lead.asked_at.is_some(),
            told: false,
        });
    }

    fn save_subagent(&self, lead: &str, agent: &str, sub: &Sub) {
        self.save(&Context {
            log: sub.tail.path().to_string_lossy().into_owned(),
            lead: lead.to_owned(),
            agent: Some(agent.to_owned()),
            tokens: sub.tokens,
            asked: false,
            told: sub.told,
        });
    }
}

/// Compacts a lead through herdr, then tells it to continue, and says how it went.
async fn compact<H: HerdrApi>(
    leads: Arc<Leads<H>>,
    mut compaction: Compacted,
    done: mpsc::UnboundedSender<Compacted>,
) {
    let name = compaction.lead.clone();
    let wait = PromptWait {
        until: vec![AgentStatus::Idle, AgentStatus::Done],
        timeout: COMPACT_TIMEOUT,
    };
    compaction.outcome = if leads.waiting(&name) {
        Outcome::Waiting
    } else {
        let result = match leads.prompt(&name, COMPACT, Some(&wait)).await {
            Ok(()) => leads.prompt(&name, CONTINUE, None).await,
            Err(e) => Err(e),
        };
        match result {
            Ok(()) => Outcome::Done,
            Err(e) => {
                eprintln!(
                    "factory: monitor: cannot compact {name}: {e}; trying again in {}s",
                    RETRY.as_secs()
                );
                Outcome::Failed
            }
        }
    };
    // A stopped steward hears nothing; the compaction still happened.
    let _ = done.send(compaction);
}

/// The message asking a lead to stop at its next natural break.
fn ask(tokens: u64) -> String {
    format!(
        "factory monitor: your context is at {}. At your next natural break (after the current \
         step finishes and its result is committed or written down), stop and wait; I will \
         compact you and then tell you to continue.",
        kilo(tokens)
    )
}

/// The message telling a lead one of its subagents passed the threshold.
fn tell(agent: &str, description: Option<&str>, tokens: u64, threshold: u64) -> String {
    let what = description.map(|d| format!(" ({d})")).unwrap_or_default();
    format!(
        "factory monitor: your subagent {agent}{what} is at {} context, past {}; hand its \
         ticket to a fresh agent at its next natural break.",
        kilo(tokens),
        kilo(threshold)
    )
}

async fn sleep_until(wake: Option<Instant>) {
    match wake {
        Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
        None => std::future::pending().await,
    }
}

fn switched(running: bool, changed: bool) -> MonitorSwitch {
    MonitorSwitch { running, changed }
}

enum Woke {
    Leads,
    Files(std::io::Result<Vec<Change>>),
    Compacted(Compacted),
    Timer,
    Stop,
}

/// What the steward's task holds.
struct State {
    policy: Policy,
    projects: PathBuf,
    leads: BTreeMap<String, Watched>,
    /// Every watched directory, by its watch.
    dirs: HashMap<i32, Dir>,
    /// Whether the link to herdr is down, so it is logged once.
    down: bool,
}

struct Dir {
    path: PathBuf,
    role: Role,
}

/// What a watched directory holds.
#[derive(Clone)]
enum Role {
    /// Session logs, the leads' among them.
    Project,
    /// The lead's session directory, where `subagents` appears.
    Session,
    /// The lead's subagents' logs.
    Subagents(String),
}

/// One lead the steward watches.
struct Watched {
    /// The session herdr reports.
    session: Option<String>,
    log: Option<PathBuf>,
    /// None until the log exists.
    tail: Option<Tail>,
    tokens: Option<u64>,
    waiting: bool,
    idle_since: Option<Instant>,
    asked_at: Option<Instant>,
    compacting: bool,
    /// Not before this, after herdr failed a prompt.
    retry_at: Option<Instant>,
    subagents: BTreeMap<String, Sub>,
    /// Subagents that finished, which the steward no longer reads.
    finished: std::collections::BTreeSet<String>,
}

impl Watched {
    fn new(log: Option<PathBuf>) -> Watched {
        Watched {
            session: None,
            log,
            tail: None,
            tokens: None,
            waiting: false,
            idle_since: None,
            asked_at: None,
            compacting: false,
            retry_at: None,
            subagents: BTreeMap::new(),
            finished: BTreeSet::new(),
        }
    }
}

struct Sub {
    tail: Tail,
    description: Option<String>,
    tokens: Option<u64>,
    told: bool,
    retry_at: Option<Instant>,
}

/// A compaction the steward started, and whether it went through.
struct Compacted {
    lead: String,
    log: Option<PathBuf>,
    outcome: Outcome,
}

/// How a compaction went.
enum Outcome {
    Done,
    /// herdr refused or failed it: it is tried again after [`RETRY`].
    Failed,
    /// The lead registered a wait first: nothing was sent.
    Waiting,
}

/// Says in the daemon's log that a session log can't be read.
fn unreadable(log: &Path, e: &std::io::Error) {
    eprintln!(
        "factory: monitor: cannot read {}: {e}; its context stays as last read until it can be, \
         check its permissions",
        log.display()
    );
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
