//! A scripted herdr in memory, for code that runs against [`HerdrApi`] inside one process.
//!
//! The binary-seam tests can't reach into the daemon, so they run a fake herdr server on a socket
//! instead (`crates/factory/tests/cli/support/herdr.rs`). Both fakes answer the same way: agents,
//! workspaces and screens as scripted, status changes told to whoever watches the pane.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use tokio::sync::mpsc;

use std::time::Duration;

use super::{
    Agent, Event, Events, HerdrApi, HerdrError, NewWorktree, PROTOCOL, Pong, Source, Watch,
    Workspace, Worktree,
};

/// A herdr that answers from a script. Every request is recorded by its method name.
#[derive(Debug)]
pub struct FakeHerdr {
    socket: PathBuf,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    protocol: u32,
    /// Answer every request with this refusal instead, as a herdr that isn't running.
    down: bool,
    agents: Vec<Agent>,
    workspaces: Vec<Workspace>,
    screens: HashMap<String, String>,
    calls: Vec<String>,
    /// Refusals scripted by method, each given to the next request of that method.
    refusals: HashMap<String, Vec<(String, String)>>,
    /// The prompts typed into agents, as (agent, text).
    prompts: Vec<(String, String)>,
    subscribers: Vec<(Vec<Watch>, mpsc::UnboundedSender<Event>)>,
}

impl Default for FakeHerdr {
    fn default() -> FakeHerdr {
        FakeHerdr::new()
    }
}

impl FakeHerdr {
    /// A running herdr on protocol [`PROTOCOL`] with no workspaces or agents.
    pub fn new() -> FakeHerdr {
        FakeHerdr {
            socket: PathBuf::from("fake-herdr.sock"),
            state: Mutex::new(State {
                protocol: PROTOCOL,
                ..State::default()
            }),
        }
    }

    pub fn protocol(&self, protocol: u32) {
        self.lock().protocol = protocol;
    }

    /// Stops answering, as a herdr that has quit: requests fail and subscriptions end.
    pub fn stop(&self) {
        let mut state = self.lock();
        state.down = true;
        state.subscribers.clear();
    }

    /// A workspace on a worktree of the repo `repo`.
    pub fn workspace(&self, id: &str, repo: &str) {
        self.lock().workspaces.push(Workspace {
            workspace_id: id.to_owned(),
            worktree: Some(Worktree {
                repo_name: repo.to_owned(),
            }),
        });
    }

    /// An agent in `pane`, whose workspace is the part of the pane id before `:`.
    pub fn agent(&self, name: Option<&str>, pane: &str, status: &str) {
        self.lock().agents.push(Agent {
            name: name.map(str::to_owned),
            agent: Some("claude".to_owned()),
            agent_status: status.to_owned(),
            pane_id: pane.to_owned(),
            workspace_id: pane.split(':').next().unwrap_or(pane).to_owned(),
        });
    }

    pub fn screen(&self, pane: &str, text: &str) {
        self.lock().screens.insert(pane.to_owned(), text.to_owned());
    }

    /// Changes the status of the agent in `pane` and tells whoever watches it.
    pub fn status(&self, pane: &str, status: &str) {
        let mut state = self.lock();
        for agent in state
            .agents
            .iter_mut()
            .filter(|agent| agent.pane_id == pane)
        {
            status.clone_into(&mut agent.agent_status);
        }
        let event = Event::AgentStatus {
            pane_id: pane.to_owned(),
            agent_status: status.to_owned(),
            agent: Some("claude".to_owned()),
        };
        tell(&mut state, &event, |watch| {
            *watch == Watch::AgentStatus(pane.to_owned())
        });
    }

    /// Closes `pane`: its agent leaves the list and whoever watches panes go hears of it.
    pub fn close(&self, pane: &str) {
        let mut state = self.lock();
        state.agents.retain(|agent| agent.pane_id != pane);
        let event = Event::PaneGone {
            pane_id: pane.to_owned(),
        };
        tell(&mut state, &event, |watch| *watch == Watch::PaneGone);
    }

    /// Makes the next request for `method` fail with herdr's error `code` and `message`.
    pub fn refuse(&self, method: &str, code: &str, message: &str) {
        self.lock()
            .refusals
            .entry(method.to_owned())
            .or_default()
            .push((code.to_owned(), message.to_owned()));
    }

    /// Every prompt typed into an agent, as (agent, text), oldest first.
    pub fn prompts(&self) -> Vec<(String, String)> {
        self.lock().prompts.clone()
    }

    /// The method of every request, oldest first.
    pub fn calls(&self) -> Vec<String> {
        self.lock().calls.clone()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Records a request, refusing it when the fake is stopped.
    fn call(&self, method: &str) -> Result<MutexGuard<'_, State>, HerdrError> {
        let mut state = self.lock();
        state.calls.push(method.to_owned());
        if state.down {
            return Err(HerdrError::Unreachable(std::io::Error::from(
                std::io::ErrorKind::ConnectionRefused,
            )));
        }
        if let Some(refusals) = state.refusals.get_mut(method)
            && !refusals.is_empty()
        {
            let (code, message) = refusals.remove(0);
            return Err(HerdrError::Refused { code, message });
        }
        Ok(state)
    }
}

fn tell(state: &mut State, event: &Event, wanted: impl Fn(&Watch) -> bool) {
    state.subscribers.retain(|(watches, events)| {
        !watches.iter().any(&wanted) || events.send(event.clone()).is_ok()
    });
}

impl HerdrApi for FakeHerdr {
    fn socket(&self) -> &Path {
        &self.socket
    }

    async fn ping(&self) -> Result<Pong, HerdrError> {
        let state = self.call("ping")?;
        Ok(Pong {
            version: "fake".to_owned(),
            protocol: state.protocol,
        })
    }

    async fn agents(&self) -> Result<Vec<Agent>, HerdrError> {
        Ok(self.call("agent.list")?.agents.clone())
    }

    async fn workspaces(&self) -> Result<Vec<Workspace>, HerdrError> {
        Ok(self.call("workspace.list")?.workspaces.clone())
    }

    async fn read_pane(&self, pane_id: &str, lines: u32) -> Result<String, HerdrError> {
        let state = self.call("pane.read")?;
        let text = state
            .screens
            .get(pane_id)
            .ok_or_else(|| HerdrError::Refused {
                code: "pane_not_found".to_owned(),
                message: format!("no pane {pane_id}"),
            })?;
        let all: Vec<&str> = text.lines().collect();
        let keep = usize::try_from(lines).unwrap_or(usize::MAX).min(all.len());
        Ok(all[all.len() - keep..].join("\n"))
    }

    async fn subscribe(&self, watches: &[Watch]) -> Result<Events, HerdrError> {
        let mut state = self.call("events.subscribe")?;
        let (sender, receiver) = mpsc::unbounded_channel();
        state.subscribers.push((watches.to_vec(), sender));
        Ok(Events {
            source: Source::Script(receiver),
        })
    }

    async fn create_worktree(
        &self,
        repo: &Path,
        _branch: &str,
        _base: &str,
        label: &str,
    ) -> Result<NewWorktree, HerdrError> {
        let mut state = self.call("worktree.create")?;
        let name = repo
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        let workspace_id = format!("w{}", state.workspaces.len() + 1);
        state.workspaces.push(Workspace {
            workspace_id: workspace_id.clone(),
            worktree: Some(Worktree {
                repo_name: name.clone(),
            }),
        });
        Ok(NewWorktree {
            pane_id: format!("{workspace_id}:p1"),
            path: format!("/worktrees/{name}/{label}"),
            workspace_id,
        })
    }

    async fn start_agent(
        &self,
        name: &str,
        pane_id: &str,
        _args: &[String],
        _timeout: Duration,
    ) -> Result<(), HerdrError> {
        let mut state = self.call("agent.start")?;
        state.agents.push(Agent {
            name: Some(name.to_owned()),
            agent: Some("claude".to_owned()),
            agent_status: "idle".to_owned(),
            pane_id: pane_id.to_owned(),
            workspace_id: pane_id.split(':').next().unwrap_or(pane_id).to_owned(),
        });
        Ok(())
    }

    async fn read_agent(&self, name: &str) -> Result<String, HerdrError> {
        let state = self.call("agent.read")?;
        let pane = state
            .agents
            .iter()
            .find(|agent| agent.name.as_deref() == Some(name))
            .map(|agent| agent.pane_id.clone())
            .unwrap_or_default();
        Ok(state.screens.get(&pane).cloned().unwrap_or_default())
    }

    async fn send_keys(&self, _name: &str, _keys: &[&str]) -> Result<(), HerdrError> {
        self.call("agent.send_keys").map(drop)
    }

    async fn wait_agent(
        &self,
        _name: &str,
        _until: &[&str],
        _timeout: Duration,
    ) -> Result<(), HerdrError> {
        self.call("agent.wait").map(drop)
    }

    async fn prompt_agent(
        &self,
        name: &str,
        text: &str,
        _until: &[&str],
        _timeout: Duration,
    ) -> Result<(), HerdrError> {
        let mut state = self.call("agent.prompt")?;
        state.prompts.push((name.to_owned(), text.to_owned()));
        for agent in state
            .agents
            .iter_mut()
            .filter(|agent| agent.name.as_deref() == Some(name))
        {
            "working".clone_into(&mut agent.agent_status);
        }
        Ok(())
    }
}
