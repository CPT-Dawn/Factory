//! The daemon's picture of the leads, kept current by herdr's events.
//!
//! At start (and again whenever the link is down and a lead command comes in) the daemon pings
//! herdr and refuses any protocol but [`PROTOCOL`]. It then lists herdr's agents and workspaces,
//! subscribes to each lead's pane on one connection it keeps open, and lists once more, so no
//! change falls between the listing and the subscription. Status changes arrive as events; an
//! agent appearing or leaving makes the daemon list again and resubscribe, since herdr can't
//! add to a live subscription. Nothing polls.
//!
//! Each lead the daemon spawns gets one `lead-end` in the ledger: `finished` once it has written
//! its report, `dead` when its agent is gone without one. A lead whose latest start already has an
//! end in the ledger, whoever logged it, is not given a second.
//!
//! A wait on a lead registers itself in the wait registry, so the lead it runs in shows as
//! waiting, and is answered when an event settles the lead it waits on.
//!
//! The registry keeps each blocking `factory wait` in the database too: a daemon that starts
//! puts back every wait the last one held, and the CLI that lost its daemon picks its wait up
//! again by id. A wait no CLI picks up within [`UNCLAIMED_GRACE`] is forgotten.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use factory_adapters::herdr::{
    Agent, AgentSession, Event, Events, Herdr, HerdrApi, HerdrError, PROTOCOL, PromptWait, Watch,
};
use factory_core::duration;
use factory_core::event;
use factory_core::exit::Exit;
use factory_core::lead::{self, AgentStatus, LeadFacts, LeadState};
use factory_core::status;
use factory_core::wait::{Wait, WaitId, WaitOn, Waits};
use tokio::sync::watch;
use tokio::task::AbortHandle;
use tokio::time::Instant;

use crate::ledger::{End, Ledger};
use crate::output::Failure;
use crate::paths::Paths;
use crate::protocol::{LeadList, LeadReport, SpawnParams, SpawnResult, WaitLeadResult};
use crate::spawn;
use crate::store::{Store, WaitRow};

/// How long a wait the last daemon held stays registered for its CLI to come back for it.
const UNCLAIMED_GRACE: Duration = Duration::from_secs(60);

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
    /// Set once the daemon stops: the waits it holds stay in the database for the next one.
    stopping: AtomicBool,
}

struct Inner {
    /// Why lead commands can't be answered; none while the subscription runs.
    down: Option<String>,
    /// Every lead seen since the daemon started, by agent name.
    leads: BTreeMap<String, Lead>,
    waits: Waits,
    /// The registered waits the database keeps, by their row.
    rows: BTreeMap<i64, WaitId>,
    /// The rows the last daemon held that no CLI has come back for yet.
    unclaimed: BTreeSet<i64>,
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
    /// Which run of its slug this is, by the count of `lead-start` events in the ledger when it
    /// began: an end is written only for the run it belongs to.
    run: usize,
}

struct Lead {
    pane: String,
    report: Option<PathBuf>,
    /// None once the agent or its pane is gone.
    agent: Option<AgentStatus>,
    cwd: Option<String>,
    session: Option<AgentSession>,
    /// When its agent's status last changed, as the daemon heard it.
    since: Instant,
}

/// A lead as the context steward sees it.
#[derive(Clone, Debug)]
pub struct Seen {
    pub name: String,
    /// None once the agent or its pane is gone.
    pub agent: Option<AgentStatus>,
    /// Whether a wait is registered in its pane.
    pub waiting: bool,
    /// Whether its final report is written.
    pub reported: bool,
    pub cwd: Option<String>,
    pub session: Option<AgentSession>,
    /// When its agent's status last changed, as the daemon heard it.
    pub since: Instant,
}

impl<H: HerdrApi> Leads<H> {
    pub fn new(
        paths: Paths,
        herdr: H,
        ledger: Arc<Ledger>,
        store: Store,
    ) -> Result<Arc<Leads<H>>, Failure> {
        let running = store.leads_running().map_err(|e| {
            Failure::precondition(format!(
                "cannot read the running leads from the database: {e}; check FACTORY_STATE"
            ))
        })?;
        // The ledger says which still owe their end: a daemon that stopped between keeping a
        // lead and writing its start, or between writing its end and keeping that, owes none.
        let mut unfinished = BTreeMap::new();
        for (name, repo, report) in running {
            let standing = ledger.standing(&repo, lead::slug(&name)).map_err(|e| {
                Failure::precondition(format!(
                    "cannot read {}: {e}; check FACTORY_STATE",
                    ledger.path.display()
                ))
            })?;
            if !standing.owes {
                if let Err(e) = store.lead_ended(&name) {
                    eprintln!("factory: cannot mark {name} ended in the database: {e}");
                }
                continue;
            }
            unfinished.insert(
                name,
                Unfinished {
                    repo,
                    report: PathBuf::from(report),
                    run: standing.starts,
                },
            );
        }
        // The blocking waits the last daemon held, until their CLIs come back for them.
        let kept = store.waits().map_err(|e| {
            Failure::precondition(format!(
                "cannot read the waits from the database: {e}; check FACTORY_STATE"
            ))
        })?;
        let mut waits = Waits::default();
        let mut rows = BTreeMap::new();
        for row in &kept {
            let id = waits.register(Wait {
                waiter: row.waiter.clone(),
                on: row.on.clone(),
            });
            rows.insert(row.id, id);
        }
        let unclaimed = rows.keys().copied().collect();
        Ok(Arc::new(Leads {
            herdr,
            paths,
            ledger,
            store: Mutex::new(store),
            inner: Mutex::new(Inner {
                down: Some("herdr not checked yet".to_owned()),
                leads: BTreeMap::new(),
                waits,
                rows,
                unclaimed,
                watched: BTreeSet::new(),
                follower: None,
                generation: 0,
                unfinished,
            }),
            turn: tokio::sync::Mutex::new(()),
            changed: watch::channel(0).0,
            stopping: AtomicBool::new(false),
        }))
    }

    /// Checks herdr at the daemon's start; a failure goes to the daemon's log, and each lead
    /// command checks again.
    pub async fn start(self: Arc<Self>) {
        tokio::spawn(Arc::clone(&self).settle_loop());
        tokio::spawn(Arc::clone(&self).forget_unclaimed());
        if let Err(failure) = self.check().await {
            eprintln!("factory: {}", failure.message);
        }
    }

    /// Sends `text` to the lead `name` as a prompt, through herdr.
    pub async fn prompt(
        &self,
        name: &str,
        text: &str,
        wait: Option<&PromptWait>,
    ) -> Result<(), HerdrError> {
        self.herdr.prompt(name, text, wait).await
    }

    /// Whether the lead `name` has a wait registered in its pane now.
    pub fn waiting(&self, name: &str) -> bool {
        let inner = self.lock();
        inner
            .leads
            .get(name)
            .is_some_and(|lead| inner.waits.waiting_on(&lead.pane).is_some())
    }

    /// Every lead seen since the daemon started, or none while the link to herdr is down.
    pub fn seen(&self) -> Option<Vec<Seen>> {
        let inner = self.lock();
        if inner.down.is_some() {
            return None;
        }
        let seen = inner
            .leads
            .iter()
            .map(|(name, lead)| Seen {
                name: name.clone(),
                agent: lead.agent,
                waiting: inner.waits.waiting_on(&lead.pane).is_some(),
                reported: lead.report.as_ref().is_some_and(|path| path.is_file()),
                cwd: lead.cwd.clone(),
                session: lead.session.clone(),
                since: lead.since,
            })
            .collect();
        Some(seen)
    }

    /// What `factory status` shows of the leads, from the picture alone: the reason the link to
    /// herdr is down, if it is, and every lead seen. Unlike [`Leads::list`] it asks herdr for
    /// nothing, not even to bring a link that is down back up.
    pub fn picture(&self) -> (Option<String>, Vec<LeadReport>) {
        let inner = self.lock();
        let leads = inner
            .leads
            .iter()
            .map(|(name, lead)| report(name, lead, &state_of(&inner, lead)))
            .collect();
        (inner.down.clone(), leads)
    }

    /// Every registered wait, oldest first, with the lead in the pane that registered it.
    pub fn registered(&self) -> Vec<status::Wait> {
        let inner = self.lock();
        inner
            .waits
            .all()
            .map(|wait| {
                let lead = wait.waiter.as_deref().and_then(|pane| {
                    inner
                        .leads
                        .iter()
                        .find(|(_, lead)| lead.pane == pane)
                        .map(|(name, _)| name.clone())
                });
                let (kind, on) = wait.on.parts();
                status::Wait {
                    pane: wait.waiter.clone(),
                    lead,
                    kind: kind.to_owned(),
                    on,
                }
            })
            .collect()
    }

    /// Ticks on every change to the picture.
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    /// Lists herdr's agents again in the background, as when one may have started a new session.
    pub fn relist(self: &Arc<Self>) {
        tokio::spawn(Arc::clone(self).refresh());
    }

    /// Starts a lead in a new herdr workspace, writes its `lead-start` event and keeps watch
    /// for its end. Nothing is asked of herdr until it is known to be there.
    pub async fn spawn(self: &Arc<Self>, params: SpawnParams) -> Result<SpawnResult, Failure> {
        let name = lead::lead_name(&params.lead).map_err(|e| Failure::usage(e.to_string()))?;
        self.check().await?;
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
        // Kept before the event is written: a daemon that stops after it still owes the end,
        // and one that stops before it owes none, which the ledger shows on restart.
        if started.is_ok() {
            self.keep(&name, &params.repo_name, &report);
        }
        let event = event::Event::from_args("lead-start", &pairs, params.session.as_deref())
            .map_err(|e| Failure::usage(e.to_string()))?;
        self.ledger.append(&event).map_err(|e| {
            Failure::precondition(format!(
                "cannot append to {}: {e}; check the disk and FACTORY_STATE",
                self.ledger.path.display()
            ))
        })?;
        let run = self
            .ledger
            .standing(&params.repo_name, slug)
            .map_err(|e| {
                Failure::precondition(format!(
                    "cannot read {}: {e}; check FACTORY_STATE",
                    self.ledger.path.display()
                ))
            })?
            .starts;
        let started = started.map_err(|failed| Failure::precondition(failed.message))?;
        {
            // The workspace is new: list herdr again so its pane is watched.
            let _turn = self.turn.lock().await;
            if let Err(failure) = self.rebuild().await {
                eprintln!("factory: {}", failure.message);
            }
        }
        // Settled only once the picture has the lead: before, it would read as gone.
        self.lock().unfinished.insert(
            name.clone(),
            Unfinished {
                repo: params.repo_name.clone(),
                report: report.clone(),
                run,
            },
        );
        self.bump();
        Ok(SpawnResult {
            lead: name,
            workspace: started.workspace,
            branch,
            worktree: started.worktree,
            report: report.to_string_lossy().into_owned(),
        })
    }

    /// Keeps `name` in the database as running, so a restarted daemon still owes its end.
    fn keep(&self, name: &str, repo: &str, report: &Path) {
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
    }

    /// Writes the `lead-end` of every spawned lead that has finished or died, once each, whenever the
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
        let done: Vec<(String, Unfinished, &'static str)> = {
            let mut inner = self.lock();
            if inner.down.is_some() {
                return;
            }
            let names: Vec<(String, &'static str)> = inner
                .unfinished
                .iter()
                .filter_map(|(name, lead)| {
                    ending(&inner, name, lead).map(|how| (name.clone(), how))
                })
                .collect();
            names
                .into_iter()
                .filter_map(|(name, how)| {
                    inner.unfinished.remove(&name).map(|lead| (name, lead, how))
                })
                .collect()
        };
        for (name, lead, how) in done {
            let pairs = [
                format!("repo={}", lead.repo),
                format!("slug={}", lead::slug(&name)),
                format!("outcome={how}"),
            ];
            // A lead-end the ledger already holds, whoever logged it, is not written again.
            let landed = event::Event::from_args("lead-end", &pairs, None)
                .map_err(|e| e.to_string())
                .and_then(|event| {
                    self.ledger
                        .append_end(&lead.repo, lead::slug(&name), lead.run, &event)
                        .map_err(|e| e.to_string())
                });
            match landed {
                // A newer run of this name has its own record: it is not this one's to close.
                Ok(End::Superseded) => {}
                Ok(End::Written | End::Already) => {
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
        self.check().await?;
        let inner = self.lock();
        let leads = inner
            .leads
            .iter()
            .map(|(name, lead)| report(name, lead, &state_of(&inner, lead)))
            .collect();
        Ok(LeadList { leads })
    }

    pub async fn status(self: &Arc<Self>, name: &str) -> Result<LeadReport, Failure> {
        self.check().await?;
        self.known(name).await?;
        let (report, state) = self.look(name)?;
        Ok(self.with_screen(report, &state).await)
    }

    /// Fails unless herdr is reachable and has a lead named `name`.
    pub async fn find(self: &Arc<Self>, name: &str) -> Result<(), Failure> {
        self.check().await?;
        self.known(name).await
    }

    /// Where the lead `name` is: its pane and, when its repo is known, its report.
    pub fn whereabouts(&self, name: &str) -> Option<(String, Option<String>)> {
        self.lock().leads.get(name).map(|lead| {
            (
                lead.pane.clone(),
                lead.report
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
            )
        })
    }

    /// [`Leads::settled`] for a wait an earlier daemon may have held on the lead that was at
    /// `pane`, reporting to `report`: one herdr no longer has went while no daemon watched it,
    /// and answers as it ended, finished with its report written and dead without.
    pub async fn resettled(
        self: &Arc<Self>,
        name: &str,
        pane: Option<String>,
        report: Option<String>,
        deadline: Instant,
        limit: Duration,
    ) -> Result<WaitLeadResult, Failure> {
        self.check().await?;
        match self.known(name).await {
            Ok(()) => self.settled(name, deadline, limit).await,
            Err(failure) => {
                let gone = {
                    let inner = self.lock();
                    inner.down.is_none() && !inner.leads.contains_key(name)
                };
                let Some(pane) = pane.filter(|_| gone) else {
                    return Err(failure);
                };
                let state = LeadState::of(LeadFacts {
                    agent: None,
                    reported: report
                        .as_ref()
                        .is_some_and(|path| Path::new(path).is_file()),
                    waiting_on: None,
                });
                let exit = state.wait_exit().unwrap_or(Exit::NeedsAttention).code();
                Ok(WaitLeadResult {
                    lead: LeadReport {
                        lead: name.to_owned(),
                        state: state.kind().to_owned(),
                        status: state.to_string(),
                        pane,
                        report,
                        lines: Vec::new(),
                    },
                    exit,
                })
            }
        }
    }

    /// Answers once the lead settles, or fails at `deadline`, `limit` after the wait began.
    /// The caller registers the wait.
    pub async fn settled(
        self: &Arc<Self>,
        name: &str,
        deadline: Instant,
        limit: Duration,
    ) -> Result<WaitLeadResult, Failure> {
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
    /// Brings the link to herdr up, or says why it can't.
    pub async fn check(self: &Arc<Self>) -> Result<(), Failure> {
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
    ///
    /// A listing is older than any event after it, so it never overrides one. A new
    /// subscription is followed by a second listing, and its events are read only once that
    /// listing is in: a change before the listing is in the listing, a change after it is an
    /// event applied after it. While the subscription stands, its events speak for the panes it
    /// watches, and a listing only adds and removes leads.
    async fn rebuild(self: &Arc<Self>) -> Result<(), Failure> {
        let (agents, repos) = self.listing().await?;
        let panes = lead_panes(&agents);
        {
            let mut inner = self.lock();
            let unchanged = inner.follower.is_some() && inner.watched == panes;
            self.take_in(&mut inner, agents, &repos);
            if unchanged {
                inner.down = None;
                drop(inner);
                self.bump();
                return Ok(());
            }
        }
        let mut watches: Vec<Watch> = panes.iter().cloned().map(Watch::AgentStatus).collect();
        watches.extend([Watch::PaneGone, Watch::AgentDetected]);
        let events = self
            .herdr
            .subscribe(&watches)
            .await
            .map_err(|e| self.fail(&e))?;
        let (agents, repos) = self.listing().await?;
        {
            let mut inner = self.lock();
            if let Some((_, old)) = inner.follower.take() {
                old.abort();
            }
            // The old subscription is gone, so this listing is the newest word on every pane.
            inner.watched = BTreeSet::new();
            self.take_in(&mut inner, agents, &repos);
            inner.generation += 1;
            let generation = inner.generation;
            let task = tokio::spawn(Arc::clone(self).follow(events, generation));
            inner.follower = Some((generation, task.abort_handle()));
            inner.watched = panes;
            inner.down = None;
        }
        self.bump();
        Ok(())
    }

    /// herdr's agents, and the repo of each workspace on a worktree.
    async fn listing(&self) -> Result<(Vec<Agent>, HashMap<String, String>), Failure> {
        let listed = async {
            Ok::<_, HerdrError>((self.herdr.agents().await?, self.herdr.workspaces().await?))
        };
        let (agents, workspaces) = listed.await.map_err(|e| self.fail(&e))?;
        let repos = workspaces
            .into_iter()
            .filter_map(|ws| Some((ws.workspace_id, ws.worktree?.repo_name)))
            .collect();
        Ok((agents, repos))
    }

    /// Takes a listing into the picture. A lead whose pane the live subscription watches keeps
    /// the status its events gave it; a lead the listing lacks is gone.
    fn take_in(&self, inner: &mut Inner, agents: Vec<Agent>, repos: &HashMap<String, String>) {
        let mut seen = BTreeSet::new();
        for agent in agents {
            let Some(name) = agent.name.filter(|name| is_lead(name)) else {
                continue;
            };
            let report = repos
                .get(&agent.workspace_id)
                .map(|repo| self.paths.lead_report(repo, lead::slug(&name)));
            let listed = agent
                .agent
                .as_ref()
                .map(|_| AgentStatus::parse(&agent.agent_status));
            let status = match inner.leads.get(&name) {
                Some(known)
                    if known.pane == agent.pane_id && inner.watched.contains(&known.pane) =>
                {
                    known.agent
                }
                _ => listed,
            };
            let since = inner
                .leads
                .get(&name)
                .filter(|old| old.agent == status)
                .map_or_else(Instant::now, |old| old.since);
            seen.insert(name.clone());
            inner.leads.insert(
                name,
                Lead {
                    pane: agent.pane_id,
                    report,
                    agent: status,
                    cwd: agent.cwd,
                    session: agent.agent_session,
                    since,
                },
            );
        }
        for (name, lead) in &mut inner.leads {
            if !seen.contains(name) && lead.agent.is_some() {
                lead.agent = None;
                lead.since = Instant::now();
            }
        }
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
                if lead.agent != agent {
                    lead.agent = agent;
                    lead.since = Instant::now();
                }
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

    /// Registers a wait, so the lead in the waiter's pane shows as waiting until the returned
    /// guard drops.
    pub fn register(self: &Arc<Self>, wait: Wait) -> Registered<H> {
        let id = self.lock().waits.register(wait);
        self.bump();
        Registered {
            leads: Arc::clone(self),
            id,
            row: None,
            answered: false,
        }
    }

    /// Registers a blocking wait and keeps it in the database, so a CLI that loses this daemon
    /// picks it up from the next by the row id [`Registered::row`] gives. `row`'s id is ignored.
    pub fn hold(self: &Arc<Self>, row: &WaitRow) -> Result<Registered<H>, Failure> {
        let kept = self.store().add_wait(row).map_err(|e| {
            Failure::precondition(format!(
                "cannot keep the wait in the database: {e}; check FACTORY_STATE, then retry"
            ))
        })?;
        let mut inner = self.lock();
        let id = inner.waits.register(Wait {
            waiter: row.waiter.clone(),
            on: row.on.clone(),
        });
        inner.rows.insert(kept, id);
        drop(inner);
        self.bump();
        Ok(Registered {
            leads: Arc::clone(self),
            id,
            row: Some(kept),
            answered: false,
        })
    }

    /// Picks up the wait the last daemon held as `row`, for the CLI that comes back for it; none
    /// when no daemon holds it for anyone: answered, forgotten, or picked up already.
    pub fn resume(self: &Arc<Self>, row: i64) -> Option<(Registered<H>, WaitRow)> {
        let id = {
            let mut inner = self.lock();
            if !inner.unclaimed.remove(&row) {
                return None;
            }
            *inner.rows.get(&row)?
        };
        let registered = Registered {
            leads: Arc::clone(self),
            id,
            row: Some(row),
            answered: false,
        };
        let kept = match self.store().waits() {
            Ok(waits) => waits.into_iter().find(|kept| kept.id == row),
            Err(e) => {
                eprintln!("factory: cannot read wait #{row} from the database: {e}");
                None
            }
        };
        // Dropping the guard forgets a row the database no longer has.
        kept.map(|kept| (registered, kept))
    }

    /// Forgets the waits the last daemon held that no CLI came back for in [`UNCLAIMED_GRACE`].
    pub async fn forget_unclaimed(self: Arc<Self>) {
        tokio::time::sleep(UNCLAIMED_GRACE).await;
        let rows: Vec<i64> = std::mem::take(&mut self.lock().unclaimed)
            .into_iter()
            .collect();
        for row in rows {
            let id = self.lock().rows.get(&row).copied();
            if let Some(id) = id {
                drop(Registered {
                    leads: Arc::clone(&self),
                    id,
                    row: Some(row),
                    answered: false,
                });
            }
        }
    }

    /// The daemon is stopping: the waits it holds stay in the database for the next one.
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
    }

    fn store(&self) -> MutexGuard<'_, Store> {
        self.store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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

/// A registered wait, cleared when it is dropped: answered, timed out or abandoned. One the
/// database keeps is forgotten there too, unless the daemon is stopping.
pub struct Registered<H: HerdrApi> {
    leads: Arc<Leads<H>>,
    id: WaitId,
    row: Option<i64>,
    /// Its CLI has the answer, so the next daemon has nothing to resume, stopping or not.
    answered: bool,
}

impl<H: HerdrApi> Registered<H> {
    /// The database's id of a blocking wait, by which its CLI resumes it.
    pub fn row(&self) -> Option<i64> {
        self.row
    }

    /// Its CLI has the answer: the database forgets it even as the daemon stops.
    pub fn answered(&mut self) {
        self.answered = true;
    }

    /// Points the wait at `on` instead, as when the pull request it named is found.
    pub fn retarget(&self, on: WaitOn) {
        if let Some(row) = self.row
            && let Err(e) = self.leads.store().retarget_wait(row, &on)
        {
            eprintln!("factory: cannot update wait #{row} in the database: {e}");
        }
        self.leads.lock().waits.retarget(self.id, on);
        self.leads.bump();
    }
}

impl<H: HerdrApi> Drop for Registered<H> {
    fn drop(&mut self) {
        {
            let mut inner = self.leads.lock();
            inner.waits.clear(self.id);
            if let Some(row) = self.row {
                inner.rows.remove(&row);
                inner.unclaimed.remove(&row);
            }
        }
        if let Some(row) = self.row
            && (self.answered || !self.leads.stopping.load(Ordering::SeqCst))
            && let Err(e) = self.leads.store().remove_wait(row)
        {
            eprintln!("factory: cannot forget wait #{row} in the database: {e}");
        }
        self.leads.bump();
    }
}

/// The panes of the leads in a listing that have an agent: the ones a subscription watches.
fn lead_panes(agents: &[Agent]) -> BTreeSet<String> {
    agents
        .iter()
        .filter(|agent| agent.agent.is_some() && agent.name.as_deref().is_some_and(is_lead))
        .map(|agent| agent.pane_id.clone())
        .collect()
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

/// How a spawned lead ended, as its `lead-end` outcome: `finished` with its report written and
/// its agent not working, `dead` with its agent gone and no report; none while it runs.
fn ending(inner: &Inner, name: &str, lead: &Unfinished) -> Option<&'static str> {
    let agent = inner.leads.get(name).and_then(|seen| seen.agent);
    let state = LeadState::of(LeadFacts {
        agent,
        reported: lead.report.is_file(),
        waiting_on: None,
    });
    match state {
        LeadState::Finished => Some("finished"),
        LeadState::Dead => Some("dead"),
        _ => None,
    }
}
