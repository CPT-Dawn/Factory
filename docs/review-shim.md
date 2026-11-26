# The review.sh shim

The harness repo's `code-review` skill ships `scripts/review.sh`, the command every skill and implementer
calls to get a cross-vendor review. It is a shim: it keeps the arguments it has always had and hands the work to
`factory review`, so the daemon takes the slot, runs the backend, writes the report files and logs the `review`
event. The shim lives in the harness repo; this page is the contract between the two. Change one side and this
page together.

## Arguments

```
review.sh [--tier hard|standard|trivial] <base> <label> [<spec>] [<previous-report>]
```

The shim checks the label (letters, digits, `_`, `-`) and the tier before it calls factory, and exits 2 with a
`code-review: ` line on stderr when either is wrong. It runs:

```
factory review [--tier <tier>] --skill <its checkout's codex/code-review/SKILL.md> <base> <label> <spec or none> [<previous-report>]
```

- The spec is a file, `none`, or an issue ref: `#12`, `12`, `owner/repo#12` or an issue URL. factory fetches
  an issue with `gh` into `<label>.spec.md`, since the reviewers' sandboxes have no network. A path with no file
  there is no spec: the review runs without one and says so on stderr.
- `--skill` is the reviewer skill every backend's prompt carries, so a worktree's review.sh runs that worktree's
  skill and no backend spends a turn loading it. Without it, factory takes `[review] skill` from its config,
  else `~/.agents/skills/code-review/SKILL.md`.
- The tier picks the chain of reviewers, as review.sh's did: a spec review (`spec-*`) is the `spec` tier, whatever the diff or tag, on its own chain; a diff of only
  docs and config (at most 400 lines), or of at most 5 lines and no test, is `trivial` unless tagged hard; else
  the caller's tier, or standard. The chains and pools are factory's config (`[review.chains]`, `config.rs`),
  review.sh's by default. Each pool has 2 slots: a busy pool passes the review to the chain's next, and the
  chain's last waits. A pool out of quota (agy's free `/usage`, or a quota error mid-run) cools down until its
  reset. A failed `claude -p` counts as out of quota when its `result` or stderr says a plan limit (`You've hit your session limit · resets 3:45pm`, `usage limit reached|<epoch>`), a rate limit (`rate_limit_error`, HTTP 429 as a status) or a message naming a reset (`resets 3:45pm`, `resets Mon 12:00am`): the pool cools until the reset named, else for an hour, and any other failure is exit 3; a mid-run hit tries the same pool's next model first when it is next in the chain. Standard and
  trivial reviews leave the Gemini pool alone while its 5-hour bucket is under 20%.
- The default pools are `agy-gemini` and `agy-claude` (agy's two quota groups), `codex` and `claude`. A model's
  group is the one whose "Models within this group" list in agy's `/usage` names it, so Argon, Carbon and Hydrogen
  draw on the Gemini group with Neon and Flash, and the 20% reserve covers them all; an answer that lists no models
  falls back to a static map. The default chains: hard is Codex high, Neon high, agy's Opus high, Claude Opus high; spec (`[review.chains] spec`) is Neon high, agy's Opus high, Codex high, Claude Opus high;
  standard is Codex medium, Neon medium, agy's Opus high, agy's Sonnet high, Claude Sonnet high; trivial is Flash high,
  Neon low, Codex low, Claude Sonnet medium. Argon is in none of them, but a chain may name it
  (`agy-gemini:gemini-4-argon-high:high`), and an old config's `agy-argon` still works: a configured pool of that
  name reads the shared group, and a chain step naming it over the default pools runs on `agy-gemini`.
- An agy pool whose usage answer can't be read (the query fails, or the answer shows no buckets for the model's
  group) is skipped with one stderr line, `factory: <pool> skipped: <why>`, and `factory status` shows its quota
  as unknown with the reason until a review learns it again.

## What `factory review` prints

On stdout, with the review done:

1. One line, never cut, even past the 200 characters of the output contract, because it ends in the report's
   path: `review <label>: <verdict> (<n> blocking, <n> advisory, <n> question) → <absolute report path>`.
   The verdict is `clean` or `findings`. A fix round adds what it settled inside the brackets, after a `;`: `; 2 fixed, 1 still present, 1 dropped`, each only when above 0.
2. Then one line per blocking finding, from the report's `FINDINGS:` ledger.

With `--json`, one object: the event's fields plus `report`, `findings` and `blocking_findings`.

Exit codes are the table in the code: 0 for a clean review, 10 (needs attention) for findings, 2 for usage (a bad
label or tier, a base that is no commit, a missing previous report), 3 for a precondition (not in a git repo, a
backend that failed, a report without its findings ledger, the daemon stopping while the review queued), 75 when
every backend is out of quota and 124 for a timeout. A failure prints `factory: code-review: <message>` on stderr. The `code-review: ` text is what
factory-stats' classifier matches; it stays.

## Files and the event

In `<state>/reviews/<repo>/`, by label, as review.sh kept them: the bundle (`.bundle.md`: commits, diff stat,
diff, spec, previous review, touched docs, coding standards, evidence), the prompt (`.prompt`), the report
(`.md`), the reviewer's run record (Codex `.jsonl`, agy `.agy.json`, Claude `.claude.json`, which factory-stats
reads), `.ledger.json` (review.sh's: `label round base from head tier pool model effort verdict report
findings`, the head a fix round diffs from) and factory's `.findings.json`. A fix round finds the previous
head in `.findings.json`, else `.ledger.json`, else the last ok `review` event naming that report.

Every run, failed ones too, logs one `review` event with review.sh's fields: `repo label round base from head
branch spec outcome verdict reason reviewer tier tier_source pool model effort tokens duration_s report stream
bundle_bytes files` and the report's `FINDINGS:` words, `from` and `head` as 8 characters. `reviewer` is the
CLI (`agy`, `codex`, `claude`), `pool` the backend. factory adds `backend`, `fallback_from`, `skipped`,
`findings` and the counts.

## What the shim prints and returns

- factory's stdout, unchanged, then `report: <path>` on a line of its own. It is the last line and nothing
  follows it. The path is read from the first line, after its last ` → `; a first line without one is a failure
  (exit 3), never a `report:` line naming nothing.
- Exit 10 becomes 0: the review ran, and the verdict is in the report. Every other code passes through.
- factory's stderr reaches the caller under review.sh's prefix: a line's leading `factory: ` (or
  `factory: code-review: `) becomes `code-review: `, so a skipped backend's note and a failure read as they did.
- When `factory` is not on PATH: `code-review: factory is not installed; run factory install`, exit 3.

## `factory wait review <label> [--timeout <duration>]`

Blocks, silent, until the review `<label>` ends, then prints the same one line `factory review` printed (for a
review that failed, `review <label>: error (<reason>)`, plus ` → <report>` when one was written). The default
timeout is 9 minutes.

- Exit 0 for a clean review; 10 for findings or a review that ended in an error; 124 when none ended in time.
  Run it again to keep waiting.
- `--json` prints the `review` event with an `exit` field.
- It waits in the repo of the directory it runs in; from a directory that is no repo it takes the label in any
  repo.
- A review that was running when the wait began is waited for. A review that already ended is answered at once
  with its last event, unless a newer run of the same label has asked for a slot since: then it waits for that
  run.

## Limits

- Codex runs headless: review.sh's herdr tab for Codex is not in `factory review`.
- review.sh's cooldown files (`~/.local/state/factory/*-cooldown`) and flock slots are not read: the daemon keeps
  each pool's cooldown and slots.
- A review whose CLI died before it logged the event leaves a `wait review` blocked until its timeout.
- The live `review.sh` serves every review on the machine. The shim goes live only once `factory` is installed.
