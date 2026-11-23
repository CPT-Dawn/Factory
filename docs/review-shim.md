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
factory review [--tier <tier>] <base> <label> <spec or none> [<previous-report>]
```

`--tier` is recorded as `tier=` on the `review` event. It does not yet pick a backend chain; the backends come
from factory's own configuration.

## What `factory review` prints

On stdout, with the review done:

1. One line, never cut, even past the 200 characters of the output contract, because it ends in the report's
   path: `review <label>: <verdict> (<n> blocking, <n> advisory, <n> question) → <absolute report path>`.
   The verdict is `clean` or `findings`.
2. Then one line per blocking finding, from the report's `FINDINGS:` ledger.

With `--json`, one object: the event's fields plus `report`, `findings` and `blocking_findings`.

Exit codes are the table in the code: 0 for a clean review, 10 (needs attention) for findings, 2 for usage, 3 for a
precondition (a label already running, no backend, a missing base), 75 when every backend is out of quota and 124
for a timeout. A failure prints `factory: code-review: <message>` on stderr. The `code-review: ` text is what
factory-stats' classifier matches; it stays.

## What the shim prints and returns

- factory's stdout, unchanged, then `report: <path>` on a line of its own. It is the last line and nothing
  follows it. The path is read from the first line, after its last ` → `; a first line without one is a failure
  (exit 3), never a `report:` line naming nothing.
- Exit 10 becomes 0: the review ran, and the verdict is in the report. Every other code passes through.
- factory's stderr passes through, so a skipped backend's note and a failure's `code-review: ` line reach the
  caller.
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

- No tier chains yet: `--tier` is recorded, not acted on. The Gemini reserve, the automatic `trivial` tier for
  small or docs-only diffs and the herdr tab for Codex are not in `factory review`.
- A review whose CLI died before it logged the event leaves a `wait review` blocked until its timeout.
- The live `review.sh` serves every review on the machine. The shim goes live only once `factory` is installed.
