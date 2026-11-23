//! A fake herdr: a server on the sandbox's herdr socket that speaks herdr's socket protocol.
//!
//! herdr answers one request per connection, except `events.subscribe`, whose connection then
//! streams events until either side hangs up. The fake does the same, so the daemon meets the
//! shape it meets in production. Tests script the agents, workspaces and screens it reports,
//! change an agent's status (which it tells its subscribers), and read back every request it got.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::Instant;

use serde_json::{Value, json};

use super::{DEADLINE, Sandbox};

/// The protocol herdr 0.9.5 speaks, and the one the fake speaks unless told otherwise.
pub const PROTOCOL: u32 = 22;

pub struct FakeHerdr {
    socket: PathBuf,
    shared: Arc<Shared>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
struct State {
    protocol: u32,
    agents: Vec<Value>,
    workspaces: Vec<Value>,
    screens: HashMap<String, String>,
    requests: Vec<Value>,
    subscribers: Vec<Subscriber>,
    next_subscriber: u64,
}

struct Subscriber {
    id: u64,
    stream: UnixStream,
    subscriptions: Vec<Value>,
}

impl FakeHerdr {
    /// Starts a fake herdr on the socket herdr uses by default under the sandbox's config dir.
    pub fn start(sandbox: &Sandbox) -> FakeHerdr {
        let dir = sandbox.path("config/herdr");
        fs::create_dir_all(&dir).expect("create the herdr config dir");
        let socket = dir.join("herdr.sock");
        let listener = UnixListener::bind(&socket).expect("bind the fake herdr socket");
        let shared = Arc::new(Shared::default());
        shared.lock().protocol = PROTOCOL;
        let serving = Arc::clone(&shared);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let shared = Arc::clone(&serving);
                thread::spawn(move || serve(&shared, stream));
            }
        });
        FakeHerdr { socket, shared }
    }

    /// The socket the fake listens on.
    pub fn socket(&self) -> &PathBuf {
        &self.socket
    }

    /// Makes the fake answer `ping` with another protocol version.
    pub fn protocol(&self, protocol: u32) {
        self.shared.lock().protocol = protocol;
    }

    /// Adds a workspace on a worktree of the repo named `repo`.
    pub fn workspace(&self, id: &str, repo: &str) {
        self.shared.lock().workspaces.push(json!({
            "workspace_id": id,
            "label": id,
            "worktree": {
                "repo_name": repo,
                "repo_key": format!("/repos/{repo}/.git"),
                "repo_root": format!("/repos/{repo}"),
                "checkout_path": format!("/worktrees/{repo}/{id}"),
                "is_linked_worktree": true,
            },
        }));
    }

    /// Adds an agent named `name` (none for an unnamed one) in `pane`, a `<workspace>:<pane>` id.
    pub fn agent(&self, name: Option<&str>, pane: &str, status: &str) {
        let workspace = pane.split(':').next().unwrap_or(pane);
        self.shared.lock().agents.push(json!({
            "name": name,
            "agent": "claude",
            "agent_status": status,
            "pane_id": pane,
            "workspace_id": workspace,
            "tab_id": format!("{workspace}:t1"),
            "terminal_id": format!("term-{pane}"),
            "focused": false,
            "revision": 0,
        }));
    }

    /// Gives the agent in `pane` a working directory and the Claude Code session `id`.
    pub fn session(&self, pane: &str, cwd: &str, id: &str) {
        for agent in self
            .shared
            .lock()
            .agents
            .iter_mut()
            .filter(|a| a["pane_id"] == pane)
        {
            agent["cwd"] = json!(cwd);
            agent["agent_session"] = json!({
                "source": "claude-hook",
                "agent": "claude",
                "kind": "id",
                "value": id,
            });
        }
    }

    /// Every `agent.prompt` the fake got: its target and text, oldest first.
    pub fn prompts(&self) -> Vec<(String, String)> {
        prompts_of(&self.shared.lock())
    }

    /// Waits until the fake has got `n` prompts and returns them, failing the test after
    /// [`DEADLINE`].
    pub fn await_prompts(&self, n: usize) -> Vec<(String, String)> {
        let start = Instant::now();
        let mut state = self.shared.lock();
        loop {
            let prompts = prompts_of(&state);
            if prompts.len() >= n {
                return prompts;
            }
            let left = DEADLINE.checked_sub(start.elapsed()).unwrap_or_else(|| {
                panic!(
                    "got {} of {n} prompts within {DEADLINE:?}: {prompts:?}",
                    prompts.len()
                )
            });
            state = self
                .shared
                .changed
                .wait_timeout(state, left)
                .expect("wait on the fake herdr")
                .0;
        }
    }

    /// What `pane.read` returns for `pane`.
    pub fn screen(&self, pane: &str, text: &str) {
        self.shared
            .lock()
            .screens
            .insert(pane.to_owned(), text.to_owned());
    }

    /// Changes the status of the agent in `pane` and tells every subscriber watching it.
    pub fn status(&self, pane: &str, status: &str) {
        let mut state = self.shared.lock();
        let mut workspace = String::new();
        for agent in state.agents.iter_mut().filter(|a| a["pane_id"] == pane) {
            agent["agent_status"] = json!(status);
            workspace = agent["workspace_id"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
        }
        let event = json!({
            "event": "pane.agent_status_changed",
            "data": {
                "pane_id": pane,
                "workspace_id": workspace,
                "agent_status": status,
                "agent": "claude",
            },
        });
        tell(&mut state, &event, |sub| {
            sub["type"] == "pane.agent_status_changed" && sub["pane_id"] == pane
        });
    }

    /// Closes `pane`: its agent leaves the list and subscribers to `pane.closed` hear of it.
    pub fn close(&self, pane: &str) {
        let mut state = self.shared.lock();
        state.agents.retain(|agent| agent["pane_id"] != pane);
        let workspace = pane.split(':').next().unwrap_or(pane);
        let event = json!({
            "event": "pane.closed",
            "data": {"type": "pane_closed", "pane_id": pane, "workspace_id": workspace},
        });
        tell(&mut state, &event, |sub| sub["type"] == "pane.closed");
    }

    /// Hangs up on every subscriber, as a herdr that stops does.
    pub fn hang_up(&self) {
        for subscriber in self.shared.lock().subscribers.drain(..) {
            let _ = subscriber.stream.shutdown(Shutdown::Both);
        }
    }

    /// The method of every request the fake got, oldest first.
    pub fn methods(&self) -> Vec<String> {
        let state = self.shared.lock();
        state
            .requests
            .iter()
            .map(|request| request["method"].as_str().unwrap_or("?").to_owned())
            .collect()
    }

    /// How many requests for `method` the fake got.
    pub fn count(&self, method: &str) -> usize {
        self.methods().iter().filter(|m| *m == method).count()
    }

    /// Waits until a live subscriber watches the agent status of `pane`, failing the test after
    /// [`DEADLINE`].
    pub fn await_watch(&self, pane: &str) {
        let start = Instant::now();
        let mut state = self.shared.lock();
        loop {
            let watched = state.subscribers.iter().any(|subscriber| {
                subscriber
                    .subscriptions
                    .iter()
                    .any(|sub| sub["type"] == "pane.agent_status_changed" && sub["pane_id"] == pane)
            });
            if watched {
                return;
            }
            let left = DEADLINE
                .checked_sub(start.elapsed())
                .unwrap_or_else(|| panic!("nobody subscribed to {pane} within {DEADLINE:?}"));
            state = self
                .shared
                .changed
                .wait_timeout(state, left)
                .expect("wait on the fake herdr")
                .0;
        }
    }
}

fn prompts_of(state: &State) -> Vec<(String, String)> {
    state
        .requests
        .iter()
        .filter(|request| request["method"] == "agent.prompt")
        .map(|request| {
            let text = |key: &str| {
                request["params"][key]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            };
            (text("target"), text("text"))
        })
        .collect()
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Writes `event` to every subscriber with a subscription `wanted` matches, dropping the ones
/// that hung up.
fn tell(state: &mut State, event: &Value, wanted: impl Fn(&Value) -> bool) {
    let line = format!("{event}\n");
    state.subscribers.retain_mut(|subscriber| {
        if !subscriber.subscriptions.iter().any(&wanted) {
            return true;
        }
        subscriber.stream.write_all(line.as_bytes()).is_ok()
    });
}

/// Answers the one request a connection carries; a subscription keeps its connection.
fn serve(shared: &Shared, stream: UnixStream) {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let request: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
    let mut state = shared.lock();
    state.requests.push(request.clone());
    let id = request["id"].clone();
    let method = request["method"].as_str().unwrap_or_default();
    let result = match method {
        "ping" => json!({"type": "pong", "version": "0.9.5", "protocol": state.protocol}),
        "agent.list" => json!({"type": "agent_list", "agents": state.agents}),
        "workspace.list" => json!({"type": "workspace_list", "workspaces": state.workspaces}),
        "pane.read" => read(&state, &request["params"]),
        "events.subscribe" => json!({"type": "subscription_started"}),
        "agent.prompt" => prompted(&state, &request["params"]),
        other => {
            let error = json!({"id": id, "error": {
                "code": "invalid_request",
                "message": format!("invalid request: unknown variant `{other}`"),
            }});
            let _ = writeln!(&stream, "{error}");
            return;
        }
    };
    let _ = writeln!(&stream, "{}", json!({"id": id, "result": result}));
    shared.changed.notify_all();
    if method != "events.subscribe" {
        return;
    }
    let Ok(writer) = stream.try_clone() else {
        return;
    };
    let subscriptions = request["params"]["subscriptions"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let id = state.next_subscriber;
    state.next_subscriber += 1;
    state.subscribers.push(Subscriber {
        id,
        stream: writer,
        subscriptions,
    });
    shared.changed.notify_all();
    drop(state);
    // herdr resets a subscribed connection that sends anything more; the fake drops it.
    let _ = reader.read_line(&mut line);
    shared
        .lock()
        .subscribers
        .retain(|subscriber| subscriber.id != id);
    shared.changed.notify_all();
}

/// Answers a prompt at once, as herdr does for an agent already in a status it waits for.
fn prompted(state: &State, params: &Value) -> Value {
    let target = &params["target"];
    let agent = state
        .agents
        .iter()
        .find(|agent| agent["name"] == *target || agent["pane_id"] == *target)
        .cloned()
        .unwrap_or(Value::Null);
    json!({"type": "agent_prompted", "agent": agent})
}

fn read(state: &State, params: &Value) -> Value {
    let pane = params["pane_id"].as_str().unwrap_or_default();
    let screen = state.screens.get(pane).cloned().unwrap_or_default();
    let lines: Vec<&str> = screen.lines().collect();
    let keep = params["lines"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(lines.len())
        .min(lines.len());
    let text = lines[lines.len() - keep..].join("\n");
    json!({"type": "pane_read", "read": {
        "pane_id": pane,
        "workspace_id": pane.split(':').next().unwrap_or(pane),
        "tab_id": "t1",
        "source": params["source"],
        "format": "text",
        "text": text,
        "revision": 0,
        "truncated": false,
    }})
}
