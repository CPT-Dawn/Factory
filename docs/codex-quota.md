# The Codex plan's quota

Reviews run on Codex, and its plan has two usage windows, a 5-hour one and a weekly one. The daemon's quota keeper keeps both, beside the Claude plan's (see [claude-quota.md](claude-quota.md)) and the reviewer cooldowns, so reviews hold off Codex when either window is nearly spent and go on to the next backend of their chain.

## The signal

`codex app-server` speaks JSON-RPC over stdio, one JSON object a line, with no `jsonrpc` field. The exchange is three messages, sent at once:

1. `{"id":1,"method":"initialize","params":{"clientInfo":{"name":"factory","title":null,"version":"0.1.0"}}}`
2. `{"method":"initialized"}`
3. `{"id":2,"method":"account/rateLimits/read"}`

The server prints notifications of its own between the replies, so lines are read until the one whose `id` is 2 and that carries no `method`. The server runs until it is stopped; the process, with its group, is killed once the answer is in or `[codex] timeout_seconds` (default 20) has passed. The exchange spends no model tokens.

The reply's `result.rateLimits` has `primary` and `secondary` windows, each `usedPercent`, `windowDurationMins` and `resetsAt` (epoch seconds), and the `planType`. A window is the 5-hour one at 300 minutes and the weekly one at 10080, whichever slot carries it; a window of another length, or whose share or reset can't be read, is left out. `result.rateLimitResetCredits` gives the reset credits: the rows marked `available`, or its `availableCount` when that is larger.

## How it works

- The keeper asks Codex when its reading is older than 5 minutes, and only when something looks: `factory status`, `factory bar`, `factory quota codex` or a review's dispatch. Looks that find the reading fresh make no call. One ask is out at a time, on a thread of its own: a look that finds it out waits for it for 8 seconds at most, so a hung Codex never makes the bar say the daemon is down.
- A failed ask counts as an ask: a Codex that is down is not pressed again within 5 minutes. The reason shows as `codex usage unreadable: <reason>` (`codex is not installed`, `it exited with status 3 before it answered`, `it did not answer within 20s`, `its answer has no rate limits`, `its answer is not JSON`) in `factory status` and `factory quota codex`, and as a muted `?` chip with the reason in the card's `error` in the bar. An unreadable Codex is never 0%, and it holds nothing: a hold that a standing reading still names is kept beside the reason.
- Dispatch holds once `[codex] hold_at_percent` (default 90) of either window is spent, until that window resets. The mark compares whole percents, as Claude's does: 89.5% shows as 90% and does not hold below 90.0, and the weekly window is as good a reason as the 5-hour one.
- A reading whose window has reset since is unknown, not 0%, and holds nothing.
- A review's chain steps over a held backend: a backend that runs on Codex cools down until the hold ends, so the router grants the next one. With none left the review exits 75 (`every review backend is out of quota (codex until <time>, held at weekly 92%); retry after <time>`). A review that waits on a hold looks again at the plan every 5 minutes. The hold is no stored cooldown: it lifts the moment the plan's windows do.
- `factory quota codex` exits 0 to dispatch (`codex: 5h 18% resets …, weekly 40% resets …; plan plus, 2 reset credits; dispatch`), or 75 naming what holds (`codex: hold: weekly 92%; hold new reviews on codex until <time>; …`).
- `factory status` shows one `codex:` row, and the review quota's Codex backends say `held at weekly 92%` under their row while it holds. `--json` shows a `codex` object with each window's reading, the plan and `reset_credits`.
- `factory bar` prints `claude 12/40 · codex 18/92 · 2 leads`. `factory bar --json` has a Codex chip of the worst window (critical while held) and a card with a meter per window, the plan and the reset credits available.
- The daemon's database keeps the last reading (table `codex_quota`, no schema bump: it is created with the rest), so a restart shows the last windows at once. A restart asks again at the first look; whether the last ask could be read is kept in memory only.

## Config

```toml
[codex]
hold_at_percent = 90   # more than 0, at most 100
timeout_seconds = 20   # at least 1: how long app-server may take before it is killed
```

History of the readings is left to the usage ledger (#71).
