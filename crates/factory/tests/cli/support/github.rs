//! A fake GitHub behind the fake `gh`: one pull request, its head, its check runs and their
//! logs, kept in files under the sandbox's `github` dir that a test rewrites as CI moves on.
//!
//! The fake answers `gh pr view <pr> --json url --jq .url`, `gh api -i` on the pull request
//! and its head's check runs (with an ETag of the body, and `304 Not Modified` when the request
//! sends it back, as GitHub does) and on its commit statuses, `gh api` on a job's logs, and `gh event listen`, which
//! refuses as an account without event subscriptions does unless a test scripts it. Each read
//! of the check runs is logged with its time and status in `github/polls`.
//!
//! For the CI watcher it also serves the repo's self-hosted runners, its queued and running
//! workflow runs, each run's jobs and its open pull requests, and can be made to fail those reads
//! as GitHub or gh does (a 403, a rate limit, a timeout).

use std::fs;
use std::path::PathBuf;

use serde_json::{Value, json};

use super::Sandbox;

/// The pull request every fake GitHub serves.
pub const PR_URL: &str = "https://github.com/acme/shop/pull/12";
/// The head commit of that pull request until a test pushes another.
pub const HEAD: &str = "c0ffee1234567890c0ffee1234567890c0ffee12";

/// What `gh event listen` says on an account without event subscriptions.
const NOT_ENABLED: &str = "event subscriptions are in preview and not enabled for this account: \
     HTTP 403: Event subscriptions are not enabled for this account \
     (https://api.github.com/event-subscriptions)";

/// GitHub refusing a token that can't read the runners.
pub const FORBIDDEN: &str =
    "printf 'HTTP/2.0 403 Forbidden\\n\\n{\"message\":\"Resource not accessible by integration\"}'
echo 'gh: Resource not accessible by integration (HTTP 403)' >&2
exit 1";
/// GitHub's primary rate limit, spent.
pub const RATE_LIMITED: &str = "printf 'HTTP/2.0 403 Forbidden\\nX-Ratelimit-Remaining: 0\\n\\n{\"message\":\"API rate limit exceeded\"}'
echo 'gh: API rate limit exceeded for user ID 1. (HTTP 403)' >&2
exit 1";
/// gh's own client giving up on GitHub.
pub const TIMED_OUT: &str = "echo 'Get \"https://api.github.com/repos/acme/shop/actions/runners?per_page=100\": net/http: request canceled (Client.Timeout exceeded while awaiting headers)' >&2
exit 1";

/// One job of a workflow run, as GitHub's jobs API lists it.
#[derive(Clone, Copy)]
pub struct Job<'a> {
    pub id: u64,
    pub name: &'a str,
    pub status: &'a str,
    pub conclusion: Option<&'a str>,
    pub created: &'a str,
    pub started: Option<&'a str>,
    pub completed: Option<&'a str>,
    pub runner: Option<&'a str>,
}

/// A check run: `(id, name, status, conclusion, app)`.
pub type Run<'a> = (u64, &'a str, &'a str, Option<&'a str>, &'a str);

pub struct FakeGitHub {
    dir: PathBuf,
}

/// One read of the check runs: when (nanoseconds since the epoch) and the HTTP status served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Poll {
    pub at_ns: u128,
    pub status: u16,
}

impl FakeGitHub {
    /// Scripts the sandbox's `gh` to serve [`PR_URL`] at [`HEAD`], with no check runs yet.
    pub fn install(sandbox: &Sandbox) -> FakeGitHub {
        let github = FakeGitHub {
            dir: sandbox.path("github"),
        };
        fs::create_dir_all(github.dir.join("logs")).expect("create the github dir");
        fs::write(github.dir.join("url"), format!("{PR_URL}\n")).expect("write the PR url");
        github.push(HEAD);
        github.checks(&[]);
        github.statuses(&[]);
        github.runners(&[]);
        github.workflow_runs("queued", &[]);
        github.workflow_runs("in_progress", &[]);
        github.latest_runs(&[]);
        github.pulls(&[]);
        github.put("no-jobs.json", &json!({"total_count": 0, "jobs": []}));
        github.put(
            "no-runs.json",
            &json!({"total_count": 0, "workflow_runs": []}),
        );
        sandbox.fake("gh", &script(&github.dir));
        github
    }

    /// Sets the repo's self-hosted runners, each `(id, name, status, busy)`.
    pub fn runners(&self, runners: &[(u64, &str, &str, bool)]) {
        let runners: Vec<Value> = runners
            .iter()
            .map(|(id, name, status, busy)| {
                json!({"id": id, "name": name, "os": "Linux", "status": status, "busy": busy})
            })
            .collect();
        self.put(
            "runners.json",
            &json!({"total_count": runners.len(), "runners": runners}),
        );
    }

    /// Sets the ids of the repo's workflow runs whose status is `status` (`queued` or
    /// `in_progress`).
    pub fn workflow_runs(&self, status: &str, ids: &[u64]) {
        let runs: Vec<Value> = ids
            .iter()
            .map(|id| json!({"id": id, "status": status, "name": "CI"}))
            .collect();
        self.put(
            &format!("runs-{status}.json"),
            &json!({"total_count": runs.len(), "workflow_runs": runs}),
        );
    }

    /// Sets the repo's latest workflow runs, whatever their status, each `(id, updated_at)`
    /// and created then too.
    pub fn latest_runs(&self, runs: &[(u64, &str)]) {
        self.latest_runs_page(1, runs);
    }

    /// Sets page `page` of the repo's latest workflow runs, as [`FakeGitHub::latest_runs`].
    pub fn latest_runs_page(&self, page: u32, runs: &[(u64, &str)]) {
        let runs: Vec<Value> = runs
            .iter()
            .map(|(id, updated)| {
                json!({"id": id, "status": "completed", "name": "CI", "updated_at": updated,
                       "created_at": updated})
            })
            .collect();
        let file = if page == 1 {
            "runs-latest.json".to_owned()
        } else {
            format!("runs-latest-{page}.json")
        };
        self.put(
            &file,
            &json!({"total_count": runs.len(), "workflow_runs": runs}),
        );
    }

    /// Sets the jobs of the workflow run `run`.
    pub fn jobs(&self, run: u64, jobs: &[Job]) {
        let jobs: Vec<Value> = jobs
            .iter()
            .map(|job| {
                json!({
                    "id": job.id,
                    "run_id": run,
                    "name": job.name,
                    "status": job.status,
                    "conclusion": job.conclusion,
                    "created_at": job.created,
                    "started_at": job.started,
                    "completed_at": job.completed,
                    "runner_name": job.runner,
                    "html_url": format!("https://github.com/acme/shop/actions/runs/{run}/job/{}", job.id),
                })
            })
            .collect();
        self.put(
            &format!("jobs-{run}.json"),
            &json!({"total_count": jobs.len(), "jobs": jobs}),
        );
    }

    /// Sets the repo's open pull requests, each `(number, branch)`, all at [`HEAD`].
    pub fn pulls(&self, pulls: &[(u64, &str)]) {
        let pulls: Vec<Value> = pulls
            .iter()
            .map(|(number, branch)| {
                json!({
                    "number": number,
                    "state": "open",
                    "html_url": format!("https://github.com/acme/shop/pull/{number}"),
                    "head": {"ref": branch, "sha": HEAD},
                })
            })
            .collect();
        self.put("pulls.json", &Value::Array(pulls));
    }

    /// Makes every read of the runners, workflow runs, jobs and open pull requests fail: `body`
    /// is shell run in their place, as [`FORBIDDEN`], [`RATE_LIMITED`] or [`TIMED_OUT`].
    pub fn fail_actions(&self, body: &str) {
        fs::write(self.dir.join("fail.sh"), body).expect("write the failure script");
    }

    /// Lets those reads answer again.
    pub fn heal_actions(&self) {
        fs::remove_file(self.dir.join("fail.sh")).expect("remove the failure script");
    }

    /// Every `gh api` path the fake was asked for that reads the runner fleet, workflow runs,
    /// jobs or open pull requests: the CI watcher's reads, and no wait's.
    pub fn watcher_reads(&self, sandbox: &Sandbox) -> Vec<String> {
        sandbox
            .calls("gh")
            .into_iter()
            .filter_map(|call| call.into_iter().find(|arg| arg.starts_with("repos/")))
            .filter(|path| path.contains("/actions/run") || path.contains("/pulls?"))
            .collect()
    }

    /// Moves the pull request's head to `sha`.
    pub fn push(&self, sha: &str) {
        self.put(
            "pull.json",
            &json!({"number": 12, "html_url": PR_URL, "head": {"sha": sha}}),
        );
    }

    /// Sets the head's check runs, each `(id, name, status, conclusion)` from GitHub Actions.
    pub fn checks(&self, runs: &[(u64, &str, &str, Option<&str>)]) {
        let runs: Vec<Run> = runs
            .iter()
            .map(|&(id, name, status, conclusion)| (id, name, status, conclusion, "github-actions"))
            .collect();
        self.runs(&runs);
    }

    /// Sets the head's check runs, each `(id, name, status, conclusion, app)`, in pages of 100
    /// as GitHub serves them.
    pub fn runs(&self, runs: &[Run]) {
        let runs: Vec<Value> = runs
            .iter()
            .map(|(id, name, status, conclusion, app)| {
                let url = format!("https://github.com/acme/shop/actions/runs/7/job/{id}");
                json!({
                    "id": id,
                    "name": name,
                    "status": status,
                    "conclusion": conclusion,
                    "html_url": url,
                    "details_url": url,
                    "app": {"slug": app},
                })
            })
            .collect();
        for page in 2..=9 {
            let _ = fs::remove_file(self.dir.join(format!("checks-{page}.json")));
        }
        let pages: Vec<&[Value]> = runs.chunks(100).collect();
        for (number, page) in pages.iter().enumerate().skip(1) {
            self.put(
                &format!("checks-{}.json", number + 1),
                &json!({"total_count": runs.len(), "check_runs": page}),
            );
        }
        self.put(
            "checks.json",
            &json!({"total_count": runs.len(), "check_runs": pages.first().copied().unwrap_or(&[])}),
        );
    }

    /// Sets the head's commit statuses, each `(context, state)`.
    pub fn statuses(&self, statuses: &[(&str, &str)]) {
        let statuses: Vec<Value> = statuses
            .iter()
            .enumerate()
            .map(|(id, (context, state))| {
                json!({
                    "id": 900 + id,
                    "context": context,
                    "state": state,
                    "target_url": format!("https://ci.example.com/acme/shop/{context}"),
                })
            })
            .collect();
        self.put(
            "status.json",
            &json!({"state": "pending", "total_count": statuses.len(), "statuses": statuses}),
        );
    }

    /// Makes `gh pr view` hang until [`FakeGitHub::answer_pr_view`].
    pub fn hold_pr_view(&self) {
        fs::write(self.dir.join("hold"), "").expect("hold gh pr view");
    }

    pub fn answer_pr_view(&self) {
        fs::remove_file(self.dir.join("hold")).expect("let gh pr view answer");
    }

    /// Stages the check runs a listen script moves in place with `mv "$G/next.json" "$G/checks.json"`.
    pub fn stage_checks(&self, runs: &[(u64, &str, &str, Option<&str>)]) {
        self.checks(runs);
        fs::rename(self.dir.join("checks.json"), self.dir.join("next.json"))
            .expect("stage the check runs");
        self.checks(&[]);
    }

    /// Sets the log of the job `id`.
    pub fn log(&self, id: u64, text: &str) {
        fs::write(self.dir.join("logs").join(id.to_string()), text).expect("write a job log");
    }

    /// Scripts `gh event listen`: `body` is shell run in place of the refusal, with the fake's
    /// dir in `$G`.
    pub fn listen(&self, body: &str) {
        fs::write(self.dir.join("listen.sh"), body).expect("write the listen script");
    }

    /// Makes `gh pr view` find no pull request.
    pub fn forget_pr(&self) {
        fs::remove_file(self.dir.join("url")).expect("remove the PR url");
    }

    /// Every read of the check runs so far, oldest first.
    pub fn polls(&self) -> Vec<Poll> {
        fs::read_to_string(self.dir.join("polls"))
            .unwrap_or_default()
            .lines()
            .map(|line| {
                let (at, status) = line.split_once(' ').expect("a poll line has two words");
                Poll {
                    at_ns: at.parse().expect("a poll time"),
                    status: status.parse().expect("a poll status"),
                }
            })
            .collect()
    }

    /// Replaces `file` whole, so the fake never reads half of it.
    fn put(&self, file: &str, body: &Value) {
        let tmp = self.dir.join(format!("{file}.tmp"));
        fs::write(&tmp, body.to_string()).expect("write a github file");
        fs::rename(tmp, self.dir.join(file)).expect("replace a github file");
    }
}

fn script(dir: &std::path::Path) -> String {
    format!(
        r#"G='{dir}'
case "$1" in
pr)
    while [ -f "$G/hold" ]; do sleep 0.05; done
    if [ -f "$G/url" ]; then cat "$G/url"; else echo 'no pull requests found for branch "work"' >&2; exit 1; fi ;;
api)
    shift
    etag_in=''; path=''
    while [ $# -gt 0 ]; do
        case "$1" in
        -H) shift; case "$1" in "If-None-Match: "*) etag_in="${{1#If-None-Match: }}" ;; esac ;;
        --hostname) shift ;;
        -i) ;;
        *) path="$1" ;;
        esac
        shift
    done
    case "$path" in
    repos/acme/shop/actions/*|repos/acme/shop/pulls\?*)
        if [ -f "$G/fail.sh" ]; then . "$G/fail.sh"; exit $?; fi ;;
    esac
    case "$path" in
    repos/acme/shop/pulls/12) file="$G/pull.json" ;;
    repos/acme/shop/pulls\?*) file="$G/pulls.json" ;;
    repos/acme/shop/actions/runners\?*) file="$G/runners.json" ;;
    repos/acme/shop/actions/runs\?status=queued*) file="$G/runs-queued.json" ;;
    repos/acme/shop/actions/runs\?status=in_progress*) file="$G/runs-in_progress.json" ;;
    repos/acme/shop/actions/runs\?per_page=*'&page='*)
        file="$G/runs-latest-${{path##*&page=}}.json"; [ -f "$file" ] || file="$G/no-runs.json" ;;
    repos/acme/shop/actions/runs\?per_page=*) file="$G/runs-latest.json" ;;
    repos/acme/shop/actions/runs/*/jobs*)
        id="${{path#repos/acme/shop/actions/runs/}}"; id="${{id%%/*}}"
        file="$G/jobs-$id.json"; [ -f "$file" ] || file="$G/no-jobs.json" ;;
    repos/acme/shop/commits/*/check-runs*'&page='*) file="$G/checks-${{path##*&page=}}.json" ;;
    repos/acme/shop/commits/*/check-runs*) file="$G/checks.json" ;;
    repos/acme/shop/commits/*/status\?*) file="$G/status.json" ;;
    repos/acme/shop/actions/jobs/*/logs)
        id="${{path%/logs}}"; id="${{id##*/}}"
        if [ -f "$G/logs/$id" ]; then cat "$G/logs/$id"; exit 0; fi
        echo 'gh: Not Found (HTTP 404)' >&2; exit 1 ;;
    *) echo "fake gh: no route for $path" >&2; exit 1 ;;
    esac
    etag="\"$(cksum < "$file" | cut -d' ' -f1)\""
    if [ "${{etag_in#W/}}" = "$etag" ]; then code=304; else code=200; fi
    # A read is logged once it is served, so a test that saw it can change the checks.
    served() {{ case "$path" in *check-runs*) echo "$at $code" >> "$G/polls" ;; esac; }}
    at="$(date +%s%N)"
    if [ "$code" = 304 ]; then
        printf 'HTTP/2.0 304 Not Modified
Etag: %s

' "$etag"
        echo 'gh: HTTP 304' >&2
        served
        exit 1
    fi
    printf 'HTTP/2.0 200 OK
Content-Type: application/json; charset=utf-8
Etag: W/%s

' "$etag"
    cat "$file"
    served ;;
event)
    if [ -f "$G/listen.sh" ]; then . "$G/listen.sh"; exit $?; fi
    echo '{NOT_ENABLED}' >&2
    exit 1 ;;
*)
    echo "fake gh: no script for: $*" >&2
    exit 127 ;;
esac"#,
        dir = dir.display()
    )
}
