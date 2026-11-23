//! The daemon's picture of the leads, kept current by herdr's events.
//!
//! At start (and again whenever the link is down and a lead command comes in) the daemon pings
//! herdr and refuses any protocol but [`PROTOCOL`]. It then lists herdr's agents and workspaces
//! once and keeps one subscribed connection open, watching each lead's pane. Status changes
//! arrive as events; an agent appearing or leaving makes the daemon list again and resubscribe,
//! since herdr can't add to a live subscription. Nothing polls.
//!
//! A wait on a lead registers itself in the wait registry, so the lead it runs in shows as
//! waiting, and is answered when an event settles the lead it waits on.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use factory_adapters::herdr::{Event, Events, Herdr, HerdrApi, HerdrError, PROTOCOL, Watch};
use factory_core::duration;
use factory_core::event;
use factory_core::exit::Exit;
use factory_core::lead::{self, AgentStatus, LeadFacts, LeadState};
use factory_core::wait::{Wait, WaitId, WaitOn, Waits};
use tokio::sync::watch;
use tokio::task::AbortHandle;
use tokio::time::Instant;

use crate::ledger::Ledger;
use crate::output::Failure;
use crate::paths::Paths;
use crate::protocol::{LeadList, LeadReport, SpawnParams, SpawnResult, WaitLeadResult};
use crate::spawn;
use crate::store::Store;

/// How many of a pane's last lines a blocked or asking lead shows.
const SCREEN_LINES: usize = 10;
/// How many lines are read to find them: the blank rows below a prompt don't count.
const READ_LINES: u32 = 60;

pub struct Leads<H: HerdrApi = Herdr> {
    herdr: H,
    paths: Paths,
    ledger: Arc<Ledger>,
    /// What a restart must not lose: the leads spawned whose end is not in the ledger yet.
    store: Mutex<Store>,
    inner: Mutex<Inner>,
    /// Held while the picture is rebuilt from herdr, so one rebuild runs at a time.
    turn: tokio::sync::Mutex<()>,
    /// Bumped on every change to the picture; waits wake on it.
    changed: watch::Sender<u64>,
}

struct Inner {
    /// Why lead commands can't be answered; none while the subscription runs.
    down: Option<String>,
    /// Every lead seen since the daemon started, by agent name.
    leads: BTreeMap<String, Lead>,
    waits: Waits,
    /// The panes the live subscription watches.
    watched: BTreeSet<String>,
    /// The task reading the live subscription, and its generation.
    follower: Option<(u64, AbortHandle)>,
    generation: u64,
    /// The leads this daemon spawned that have not finished, by agent name: each gets one
    /// `lead-end` event when it does.
    unfinished: BTreeMap<String, Unfinished>,
}

struct Unfinished {
    repo: String,
    report: PathBuf,
    /// Read back from the database after a restart: its end may already be in the ledger, if the
    /// daemon stopped between writing it and keeping that.
    recovered: bool,
}

struct Lead {
    pane: String,
    report: Option<PathBuf>,
    /// None once the agent or its pane is gone.
    agent: Option<AgentStatus>,
}

impl<H: HerdrApi> Leads<H> {
    pub fn new(
        paths: Paths,
        herdr: H,
        ledger: Arc<Ledger>,
        store: Store,
    ) -> Result<Arc<Leads<H>>, Failure> {
        let unfinished = store
            .leads_running()
            .map_err(|e| {
                Failure::precondition(format!(
                    "cannot read the running leads from the database: {e}; check FACTORY_STATE"
                ))
            })?
            .into_iter()
            .map(|(name, repo, report)| {
                (
                    name,
                    Unfinished {
                        repo,
                        report: PathBuf::from(report),
                        recovered: true,
                    },
                )
            })
            .collect();
        Ok(Arc::new(Leads {
            herdr,
            paths,
            ledger,
            store: Mutex::new(store),
            inner: Mutex::new(Inner {
                down: Some("herdr not checked yet".to_owned()),
                leads: BTreeMap::new(),
                waits: Waits::default(),
                watched: BTreeSet::new(),
                follower: None,
                generation: 0,
                unfinished,
            }),
            turn: tokio::sync::Mutex::new(()),
            changed: watch::channel(0).0,
        }))
    }

    /// Checks herdr at the daemon's start; a failure goes to the daemon's log, and each lead
    /// command checks again.
    pub async fn start(self: Arc<Self>) {
        tokio::spawn(Arc::clone(&self).settle_loop());
        if let Err(failure) = self.up().await {
            eprintln!("factory: {}", failure.message);
        }
    }

    /// Starts a lead in a new herdr workspace, writes its `lead-start` event and keeps watch
    /// for its end. Nothing is asked of herdr until it is known to be there.
    pub async fn spawn(self: &Arc<Self>, params: SpawnParams) -> Result<SpawnResult, Failure> {
        let name = lead::lead_name(&params.lead).map_err(|e| Failure::usage(e.to_string()))?;
        self.up().await?;
        let slug = lead::slug(&name);
        let branch = lead::branch(slug);
        let report = self.paths.lead_report(&params.repo_name, slug);
        // A report left by an earlier lead of this name would read as this one finished.
        prepare(&report)?;
        let prompt = lead::prompt(
            &params.skill,
            &params.spec,
            &branch,
            &report.to_string_lossy(),
        );
        let plan = spawn::Plan {
            name: &name,
            repo: Path::new(&params.repo_path),
            branch: &branch,
            base: &params.base,
            label: slug,
            claude_args: &params.claude_args,
            prompt: &prompt,
            pause: Duration::from_millis(params.retry_pause_ms),
        };
        let started = spawn::start(&self.herdr, &plan).await;
        let (outcome, reason, stalls) = match &started {
            Ok(started) => ("ok", "", started.stalls),
            Err(failed) => ("error", failed.message.as_str(), failed.stalls),
        };
        let pairs = [
            ("repo", params.repo_name.as_str()),
            ("slug", slug),
            ("spec", params.spec.as_str()),
            ("skill", params.skill.as_str()),
            ("base", params.base.as_str()),
            ("branch", branch.as_str()),
            ("outcome", outcome),
            ("reason", reason),
            ("stalls", &stalls.to_string()),
            ("model", params.model.as_str()),
            ("effort", params.effort.as_str()),
        ]
        .map(|(key, value)| format!("{key}={value}"));
        // Kept before the event is written: a daemon that stops in between still owes the end.
        if started.is_ok() {
            self.expect_end(&name, &params.repo_name, &report, false);
        }
        let event = event::Event::from_args("lead-start", &pairs, params.session.as_deref())
            .map_err(|e| Failure::usage(e.to_string()))?;
        self.ledger.append(&event).map_err(|e| {
            Failure::precondition(format!(
                "cannot append to {}: {e}; check the disk and FACTORY_STATE",
                self.ledger.path.display()
            ))
        })?;
        let started = started.map_err(|failed| Failure::precondition(failed.message))?;
        {
            // The workspace is new: list herdr again so its pane is watched.
            let _turn = self.turn.lock().await;
            if let Err(failure) = self.rebuild().await {
                eprintln!("factory: {}", failure.message);
            }
        }
        self.resync().await;
        Ok(SpawnResult {
            lead: name,
            workspace: started.workspace,
            branch,
            worktree: started.worktree,
            report: report.to_string_lossy().into_owned(),
        })
    }

    /// Lists the agents again, now that the subscription is live. The listing a rebuild took is
    /// older than its subscription, so a lead that finished in between would show as working
    /// for good, and its end would never be written.
    async fn resync(&self) {
        if let Ok(agents) = self.herdr.agents().await {
            let mut inner = self.lock();
            for agent in agents {
                let Some(lead) = agent.name.and_then(|name| inner.leads.get_mut(&name)) else {
                    continue;
                };
                lead.agent = agent
                    .agent
                    .as_ref()
                    .map(|_| AgentStatus::parse(&agent.agent_status));
            }
            drop(inner);
            self.bump();
        }
    }

    /// Remembers that `name` is running and owes the ledger a `lead-end`.
    fn expect_end(&self, name: &str, repo: &str, report: &Path, recovered: bool) {
        let kept = self
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .lead_started(name, repo, &report.to_string_lossy());
        if let Err(e) = kept {
            eprintln!(
                "factory: cannot keep {name} in the database: {e}; its end is lost on a restart"
            );
        }
        self.lock().unfinished.insert(
            name.to_owned(),
            Unfinished {
                repo: repo.to_owned(),
                report: report.to_owned(),
                recovered,
            },
        );
    }

    /// Writes the `lead-end` of every spawned lead that has finished, once each, whenever the
    /// picture changes.
    async fn settle_loop(self: Arc<Self>) {
        let mut changes = self.changed.subscribe();
        loop {
            self.settle();
            if changes.changed().await.is_err() {
                return;
            }
        }
    }

    fn settle(&self) {
        let done: Vec<(String, Unfinished)> = {
            let mut inner = self.lock();
            if inner.down.is_some() {
                return;
            }
            let names: Vec<String> = inner
                .unfinished
                .iter()
                .filter(|(name, lead)| ended(&inner, name, lead))
                .map(|(name, _)| name.clone())
                .collect();
            names
                .into_iter()
                .filter_map(|name| inner.unfinished.remove(&name).map(|lead| (name, lead)))
                .collect()
        };
        for (name, lead) in done {
            let pairs = [
                format!("repo={}", lead.repo),
                format!("slug={}", lead::slug(&name)),
                "outcome=finished".to_owned(),
            ];
            let landed = if lead.recovered && self.ledger.has_ended(&lead.repo, lead::slug(&name)) {
                Ok(())
            } else {
                event::Event::from_args("lead-end", &pairs, None)
                    .map_err(|e| e.to_string())
                    .and_then(|event| {
                        self.ledger
                            .append(&event)
                            .map(|_| ())
                            .map_err(|e| e.to_string())
                    })
            };
            match landed {
                Ok(_) => {
                    let kept = self
                        .store
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .lead_ended(&name);
                    if let Err(e) = kept {
                        eprintln!("factory: cannot mark {name} ended in the database: {e}");
                    }
                }
                Err(e) => {
                    eprintln!("factory: cannot write the lead-end of {name}: {e}");
                    self.lock().unfinished.insert(name, lead);
                }
            }
        }
    }

    pub async fn list(self: &Arc<Self>) -> Result<LeadList, Failure> {
        self.up().await?;
        let inner = self.lock();
        let leads = inner
            .leads
            .iter()
            .map(|(name, lead)| report(name, lead, &state_of(&inner, lead)))
            .collect();
        Ok(LeadList { leads })
    }

    pub async fn status(self: &Arc<Self>, name: &str) -> Result<LeadReport, Failure> {
        self.up().await?;
        self.known(name).await?;
        let (report, state) = self.look(name)?;
        Ok(self.with_screen(report, &state).await)
    }

    /// Answers once the lead settles: finished, blocked, asking or dead.
    pub async fn wait(
        self: &Arc<Self>,
        name: &str,
        waiter: Option<String>,
        limit: Duration,
    ) -> Result<WaitLeadResult, Failure> {
        self.up().await?;
        self.known(name).await?;
        let deadline = Instant::now() + limit;
        let _wait = self.register(Wait {
            waiter,
            on: WaitOn::Lead(name.to_owned()),
        });
        let mut changes = self.changed.subscribe();
        loop {
            changes.borrow_and_update();
            let (report, state) = self.look(name)?;
            if let Some(exit) = state.wait_exit() {
                return Ok(WaitLeadResult {
                    lead: self.with_screen(report, &state).await,
                    exit: exit.code(),
                });
            }
            tokio::select! {
                _ = changes.changed() => {}
                () = tokio::time::sleep_until(deadline) => {
                    return Err(Failure::new(
                        Exit::Timeout,
                        format!(
                            "{name} is still {} after {}; wait again, or see factory lead status {}",
                            state.kind(),
                            duration::show(limit),
                            lead::slug(name),
                        ),
                    ));
                }
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn bump(&self) {
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }

    /// Brings the link up unless it is: checks herdr's protocol, then rebuilds the picture.
    async fn up(self: &Arc<Self>) -> Result<(), Failure> {
        if self.lock().down.is_none() {
            return Ok(());
        }
        let _turn = self.turn.lock().await;
        if self.lock().down.is_none() {
            return Ok(());
        }
        let pong = self.herdr.ping().await.map_err(|e| self.fail(&e))?;
        if pong.protocol != PROTOCOL {
            let failure = Failure::precondition(format!(
                "herdr at {} speaks protocol {}, this factory speaks {PROTOCOL}; \
                 install the factory release that matches herdr",
                self.herdr.socket().display(),
                pong.protocol
            ));
            self.lock().down = Some(failure.message.clone());
            return Err(failure);
        }
        self.rebuild().await
    }

    /// Rebuilds the picture when an agent appeared or left. Boxed, because the task it runs in
    /// is spawned from the subscription that [`Leads::rebuild`] spawns.
    fn refresh(self: Arc<Self>) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move {
            let _turn = self.turn.lock().await;
            if self.lock().down.is_some() {
                return;
            }
            if let Err(failure) = self.rebuild().await {
                eprintln!("factory: {}", failure.message);
            }
        })
    }

    /// Makes sure herdr has a lead named `name`, listing herdr's agents again once if the
    /// picture has none: an agent may have appeared without an event the daemon heard.
    async fn known(self: &Arc<Self>, name: &str) -> Result<(), Failure> {
        if self.lock().leads.contains_key(name) {
            return Ok(());
        }
        {
            let _turn = self.turn.lock().await;
            self.rebuild().await?;
        }
        if self.lock().leads.contains_key(name) {
            Ok(())
        } else {
            Err(no_lead(name))
        }
    }

    /// Lists herdr's agents and workspaces into the picture and subscribes to the lead panes,
    /// replacing the subscription when they changed. The caller holds the turn.
    async fn rebuild(self: &Arc<Self>) -> Result<(), Failure> {
        let listed = async {
            Ok::<_, HerdrError>((self.herdr.agents().await?, self.herdr.workspaces().await?))
        };
        let (agents, workspaces) = listed.await.map_err(|e| self.fail(&e))?;
        let repos: HashMap<String, String> = workspaces
            .into_iter()
            .filter_map(|ws| Some((ws.workspace_id, ws.worktree?.repo_name)))
            .collect();
        let panes = {
            let mut inner = self.lock();
            let mut seen = BTreeSet::new();
            for agent in agents {
                let Some(name) = agent.name.filter(|name| is_lead(name)) else {
                    continue;
                };
                let report = repos
                    .get(&agent.workspace_id)
                    .map(|repo| self.paths.lead_report(repo, lead::slug(&name)));
                let status = agent
                    .agent
                    .as_ref()
                    .map(|_| AgentStatus::parse(&agent.agent_status));
                seen.insert(name.clone());
                inner.leads.insert(
                    name,
                    Lead {
                        pane: agent.pane_id,
                        report,
                        agent: status,
                    },
                );
            }
            for (name, lead) in &mut inner.leads {
                if !seen.contains(name) {
                    lead.agent = None;
                }
            }
            let panes: BTreeSet<String> = inner
                .leads
                .values()
                .filter(|lead| lead.agent.is_some())
                .map(|lead| lead.pane.clone())
                .collect();
            if inner.follower.is_some() && inner.watched == panes {
                inner.down = None;
                drop(inner);
                self.bump();
                return Ok(());
            }
            panes
        };
        let mut watches: Vec<Watch> = panes.iter().cloned().map(Watch::AgentStatus).collect();
        watches.extend([Watch::PaneGone, Watch::AgentDetected]);
        let events = self
            .herdr
            .subscribe(&watches)
            .await
            .map_err(|e| self.fail(&e))?;
        {
            let mut inner = self.lock();
            inner.generation += 1;
            let generation = inner.generation;
            if let Some((_, old)) = inner.follower.take() {
                old.abort();
            }
            let task = tokio::spawn(Arc::clone(self).follow(events, generation));
            inner.follower = Some((generation, task.abort_handle()));
            inner.watched = panes;
            inner.down = None;
        }
        self.bump();
        Ok(())
    }

    /// Reads one subscription until herdr hangs up or a newer one replaces it.
    async fn follow(self: Arc<Self>, mut events: Events, generation: u64) {
        while let Some(event) = events.next().await {
            match event {
                Event::AgentStatus {
                    pane_id,
                    agent_status,
                    agent,
                } => self.update(&pane_id, agent.map(|_| AgentStatus::parse(&agent_status))),
                Event::PaneGone { pane_id } => self.update(&pane_id, None),
                Event::AgentDetected { .. } => {
                    tokio::spawn(Arc::clone(&self).refresh());
                }
                Event::Other(_) => {}
            }
        }
        {
            let mut inner = self.lock();
            if inner.follower.as_ref().map(|(g, _)| *g) != Some(generation) {
                return;
            }
            inner.follower = None;
            inner.down = Some(format!(
                "lost herdr's events at {}; retry once herdr runs",
                self.herdr.socket().display()
            ));
        }
        self.bump();
    }

    fn update(&self, pane: &str, agent: Option<AgentStatus>) {
        {
            let mut inner = self.lock();
            for lead in inner.leads.values_mut().filter(|lead| lead.pane == pane) {
                lead.agent = agent;
            }
        }
        self.bump();
    }

    /// The lead's report and state now, or why the picture can't say.
    fn look(&self, name: &str) -> Result<(LeadReport, LeadState), Failure> {
        let inner = self.lock();
        if let Some(down) = &inner.down {
            return Err(Failure::precondition(down.clone()));
        }
        let lead = inner.leads.get(name).ok_or_else(|| no_lead(name))?;
        let state = state_of(&inner, lead);
        Ok((report(name, lead, &state), state))
    }

    /// Adds the pane's last lines to the report of a lead that is blocked or asking.
    async fn with_screen(&self, mut report: LeadReport, state: &LeadState) -> LeadReport {
        if !state.shows_screen() {
            return report;
        }
        report.lines = match self.herdr.read_pane(&report.pane, READ_LINES).await {
            Ok(text) => last_lines(&text),
            Err(e) => vec![format!("(cannot read pane {}: {e})", report.pane)],
        };
        report
    }

    fn register(self: &Arc<Self>, wait: Wait) -> Registered<H> {
        let id = self.lock().waits.register(wait);
        self.bump();
        Registered {
            leads: Arc::clone(self),
            id,
        }
    }

    /// A failure talking to herdr: the link goes down, and the next lead command retries.
    fn fail(&self, e: &HerdrError) -> Failure {
        let socket = self.herdr.socket().display();
        let message = match e {
            HerdrError::Unreachable(e) => format!(
                "herdr unreachable at {socket}: {e}; start herdr, or set HERDR_SOCKET_PATH to its socket"
            ),
            other => format!("herdr at {socket} failed: {other}; retry"),
        };
        let mut inner = self.lock();
        if let Some((_, follower)) = inner.follower.take() {
            follower.abort();
        }
        inner.down = Some(message.clone());
        Failure::precondition(message)
    }
}

/// A registered wait, cleared when it is dropped: answered, timed out or abandoned.
struct Registered<H: HerdrApi> {
    leads: Arc<Leads<H>>,
    id: WaitId,
}

impl<H: HerdrApi> Drop for Registered<H> {
    fn drop(&mut self) {
        self.leads.lock().waits.clear(self.id);
        self.leads.bump();
    }
}

/// The last [`SCREEN_LINES`] lines of a pane's text, after dropping the blank rows at its end.
fn last_lines(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let end = lines
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .map_or(0, |i| i + 1);
    let start = end.saturating_sub(SCREEN_LINES);
    lines[start..end]
        .iter()
        .map(|line| (*line).to_owned())
        .collect()
}

/// Whether an agent's name is one a lead can have, so every lead listed can be named.
fn is_lead(name: &str) -> bool {
    lead::lead_name(name).is_ok_and(|named| named == name)
}

fn no_lead(name: &str) -> Failure {
    Failure::precondition(format!(
        "herdr has no lead named {name}; see factory lead list"
    ))
}

fn state_of(inner: &Inner, lead: &Lead) -> LeadState {
    let waiting_on = inner.waits.waiting_on(&lead.pane).map(ToString::to_string);
    LeadState::of(LeadFacts {
        agent: lead.agent,
        reported: lead.report.as_ref().is_some_and(|path| path.is_file()),
        waiting_on: waiting_on.as_deref(),
    })
}

fn report(name: &str, lead: &Lead, state: &LeadState) -> LeadReport {
    LeadReport {
        lead: name.to_owned(),
        state: state.kind().to_owned(),
        status: state.to_string(),
        pane: lead.pane.clone(),
        report: lead
            .report
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
        lines: Vec::new(),
    }
}

/// Clears what an earlier lead of the same name left, and makes the directory its report goes in.
fn prepare(report: &Path) -> Result<(), Failure> {
    let cannot = |e: std::io::Error| {
        Failure::precondition(format!(
            "cannot prepare the report {}: {e}; check FACTORY_STATE",
            report.display()
        ))
    };
    match std::fs::remove_file(report) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(cannot(e)),
    }
    if let Some(dir) = report.parent() {
        std::fs::create_dir_all(dir).map_err(cannot)?;
    }
    Ok(())
}

/// Whether a spawned lead is over: its report is written and its agent isn't working, or its
/// agent is gone with the report. A lead that died without one never ends here.
fn ended(inner: &Inner, name: &str, lead: &Unfinished) -> bool {
    let agent = inner.leads.get(name).and_then(|seen| seen.agent);
    let state = LeadState::of(LeadFacts {
        agent,
        reported: lead.report.is_file(),
        waiting_on: None,
    });
    state == LeadState::Finished
}
