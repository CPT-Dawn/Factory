# The review.sh shim

The harness repo's `code-review` skill ships `scripts/review.sh`, the command every skill and implementer
calls to get a cross-vendor review. It is a shim: it keeps the arguments it has always had and hands the work to
`factory review`, so the daemon takes the slot, runs the backend, writes the report files and logs the `review`
event. The shim lives in the harness repo; this page is the contract between the two. Change one side and this
page together.

## Arguments

```
review.sh [--tier hard|standard|trivial] [--escalation] <base> <label> [<spec>] [<previous-report>]
```

(`--escalation` is passed through to factory; a review.sh older than the round cap, docs/adr/0002, lacks it.
`factory review` also takes `--standalone` and `--follow-up <issue>`, below; a review.sh that does not pass
them through cannot run a retro's review or a follow-up's.)

The shim checks the label (letters, digits, `_`, `-`) and the tier before it calls factory, and exits 2 with a
`code-review: ` line on stderr when either is wrong. It runs:

```
factory review [--tier <tier>] [--escalation] --skill <its checkout's codex/code-review/SKILL.md> <base> <label> <spec or none> [<previous-report>]
```

- The label's `-rN` suffix is the round, and factory holds the cap (docs/adr/0002): rounds 1 and 2 always run;
  rounds 3 and 4 only with `--escalation` (implement-spec's Opus re-dispatch); after the last round, 2 or 4, one
  light review, `<prefix>-rN-light`, reviews only the last fix, from round N's head, once. Any other round
  (`-r3` without `--escalation`, `-r5`, a light review after round 1 or 3, a second light review) is refused
  with exit 2 and one line saying to report what is still open; `--escalation` on rounds 1 and 2 is refused too.
- A label that names no round (`t1`, `retro-lab-light2`, `t1-r5-light-2`, a zero-padded `t1-r02`, an empty
  prefix `-r1`) is refused unless the review is
  `--standalone`, a review outside a ticket such as a retro's; `--standalone` with a `-rN` label is refused.
- `--escalation` needs an `escalation` event naming the prefix in this repo's ledger, which the lead logs when it
  re-dispatches the ticket: `factory log escalation repo=<repo> prefix=<prefix>`. Without one it is refused.
- A new round 1, or a standalone review, of work a prefix has reviewed to its cap (its round 2 with no
  escalation, its round 4, or its light review) is refused, with a line saying to file a follow-up issue for
  what is still open. The same work is the same head, or the same tree reviewed from the same base commit,
  however spelled (an empty commit on top). `--follow-up <issue>` names that new issue, which gh must be able
  to view and whose URL must not be the spec's; only a round 1 takes it. Its round 1 then runs, and its `review` event carries `follow_up`.
- Round N > 1 and a light review continue the round before (`<prefix>-r<N-1>`, or round N for `-rN-light`):
  without a previous report, factory takes that round's report from `<state>/reviews/<repo>/`, and refuses with
  exit 2, naming the missing report, when there is none. It never reviews such a round fresh.

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
  trivial reviews leave the Gemini pool alone while its 5-hour bucket is under 20%. An agy pool holds, as Claude and
  Codex do, once `[agy] hold_at_percent` (default 90, more than 0 and at most 100) of either bucket of its group is
  spent, until that bucket resets: it cools down and the chain's next takes the review. Each reading the daemon takes
  of agy's usage (`factory status`, `factory bar`, `factory quota agy`) joins the usage history as harness `agy`,
  one row per group and touched window, as its whole percent; a review's own usage query runs in the review's
  process and is not the daemon's reading.
- The default pools are `agy-gemini` and `agy-claude` (agy's two quota groups), `codex` and `claude`. A model's
  group is the one whose "Models within this group" list in agy's `/usage` names it, so Argon, Carbon and Hydrogen
  draw on the Gemini group with Neon and Flash, and the 20% reserve covers them all; an answer that lists no models
  falls back to a static map. The default chains: hard is Codex high, Neon high, agy's Opus high, Claude Opus high; spec (`[review.chains] spec`) is Neon high, agy's Opus high, Codex high, Claude Opus high, the former built-in hard chain; a config that sets a `hard` chain and no `spec` chain runs spec reviews on its `hard` chain, as before the spec tier existed;
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
head in `.findings.json`, else `.ledger.json`, else the last ok `review` event naming that report. A previous
head that is not on the branch (an escalation on a new branch) makes the round review the whole diff, with a line
on stderr, but the previous round's open findings still go to the reviewer and its ledger marks them.

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
