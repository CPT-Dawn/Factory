# The Claude plan's quota

At 08:30 IST on 2026-11-23 the plan's 5-hour limit (HTTP 429) ended six subagents mid-ticket. The daemon's quota keeper now keeps the Claude plan's two usage windows, the 5-hour and the weekly, beside the reviewer quotas, so a lead holds new dispatches when either window is nearly spent and resumes once it resets.

## Signals, and the one chosen

| Signal | Documented | Gives | Used |
|---|---|---|---|
| Statusline stdin JSON, `rate_limits.five_hour` and `rate_limits.seven_day`, each `used_percentage` and `resets_at` (epoch seconds) | yes: [statusline](https://code.claude.com/docs/en/statusline) | share spent and reset, Pro/Max only, after a session's first answer | **primary** |
| `StopFailure` hook, `error: "rate_limit"` and `last_assistant_message` | yes: [hooks](https://code.claude.com/docs/en/hooks) | that a turn hit a limit; the reset only as message text | **fallback** |
| Limit messages: `You've hit your session limit · resets 3:45pm`, `... weekly limit · resets Mon 12:00am` | yes: [errors](https://code.claude.com/docs/en/errors) | a local wall-clock reset with no date or zone | parsed by the fallback |
| `/usage` | interactive only: [commands](https://code.claude.com/docs/en/commands) | nothing a daemon can read | no |
| `anthropic-ratelimit-unified-*` response headers | no | n/a | no |
| Transcript JSONL fields | no schema: [sessions](https://code.claude.com/docs/en/sessions) | the error text | no |

The statusline is the only documented signal that gives both the share spent and the reset. The hook catches a 429 that arrives before any statusline has run, or on a machine without one.

## How it works

- `factory hook statusline` takes the status line's JSON. The status line command (glowline) hands it its own stdin on every refresh. The hook sends both windows, `five_hour` and `seven_day`, to a daemon that is already running and prints nothing. It exits 0 at once on any input (valid, partial, empty or garbage) and when the daemon is down, and it never starts a daemon, not even through systemd's socket: it calls only on the live socket (`live.sock`, beside `factory.sock` in the runtime dir and no longer a name), which the running daemon binds itself beside systemd's and removes as it stops, so with no daemon the call is refused. A status line must not fail or wait. A window the input lacks or garbles (a share outside 0 to 100, a reset before the epoch or past the year 9999) keeps its last reading.
- `factory hook stop-failure` is the `StopFailure` hook. A turn that ended on a rate limit records a limit until the reset its message names. Without a named reset, the limit lasts until the 5-hour window's known reset, else one hour. A limit that is already holding keeps its reset unless a later one is named. Another error counts only when its message names a plan limit; otherwise it changes nothing.
- Once both windows have been read since a limit was recorded, each standing and with room (under 100%), the limit lifts, because that 429 was a capacity limit and not the plan's. A window not read since the limit, or read past its reset, is no evidence of room.
- Dispatch holds once `[claude] hold_at_percent` (default 90) of either window is spent, until that window resets; with both over the mark, until the later reset. A limit holds as well, and the later of the two holds wins.
- A reading whose window has reset since is unknown, not 0%: `5h unknown (seen 3h ago)`. It holds nothing.
- `factory quota claude` exits 0 to dispatch, naming both windows (`claude: 5h 40% resets …, weekly 12% resets …; dispatch`), or 75 naming what holds and every window read so far, a stale one with its age: `claude: hold: weekly 91%; hold new dispatches until <time>; 5h unknown (seen 3h ago), weekly 91%; …`, or `claude: hit its limit; …`.
- `factory wait claude [--timeout 9m]` blocks, silent, until a lead may dispatch (exit 0), or exits 124 to wait again.
- `factory status` shows one `claude:` row (`claude: hold: weekly 91% until 11-30 02:00Z; 5h 41%, weekly 91%`), and `--json` shows a `claude` object with each window's reading.
- `factory bar` prints `claude 12/40 · agy 30/63 · 2 leads` (5-hour/weekly, `?` for a window not standing; agy's is the worst of its quota groups), and `factory bar --json` the bar feed with a Claude chip and card, a meter per window. Like the hook, the bar asks only a daemon that runs, and starts none. Only an unreachable daemon gives the bar's exit-0 "factory daemon down" document; a bad config exits 3 as usual.
- The daemon's database keeps the quota (table `claude_quota`, with the weekly window's columns since schema 6), so a restart forgets no hold. A schema 5 store migrates in place.

## agy's quota (spec #67)

agy has no hook: the daemon's quota keeper asks `agy -p /usage --output-format json` itself, and shows what it read for five minutes, whoever looks (`factory status`, `factory bar`, `factory quota agy`). Routing keeps its own, separate 120 s freshness check on a file cache, so a dispatch never decides on a five-minute-old answer. A query takes seconds, so one runs on its own and at most once at a time; status and the bar wait four seconds for it and show the last reading (or `agy: no reading yet`) when it is slower.

- Each quota group (`Gemini Models`, `Claude and GPT models`) has a 5-hour and a weekly window. A bucket that agy reports with the Unix epoch as its reset has no clock running: with a remaining fraction of 1 it is an unused window, 0%; with a lower fraction it is unknown, and routing treats the answer as unreadable. It is never "a reset that passed".
- An answer that is garbage, or a query that fails, shows `agy usage unreadable: <why>` in `factory status`, `factory quota agy` and the bar (a muted `?` chip, and the card's `error`), never 0% or 100%. It is asked again after five minutes like any reading.
- `factory quota agy` prints `agy: Gemini Models: 5h 30%, weekly 7%; Claude and GPT models: 5h 24%, weekly 63%` and exits 0, reading or not: it reports, and does not gate. `--json` has `groups`, `error` and `seen`.
- The bar's agy chip shows the worst window across the groups, toned by its share. The card has one block per quota group: a text row naming it, then a meter per window.
- The keeper holds the reading in memory: a restart asks again.

## Wiring (DotFiles)

`~/.claude/settings.json`:

```json
"statusLine": {"type": "command", "command": "glowline"},
"hooks": {"StopFailure": [{"matcher": "rate_limit", "hooks": [{"type": "command", "command": "factory hook stop-failure"}]}]}
```

glowline pipes the JSON it reads to `factory hook statusline` and draws the line itself.

implement-spec runs `factory quota claude` before each dispatch. On exit 75 it runs `factory wait claude` until that exits 0, then dispatches.
