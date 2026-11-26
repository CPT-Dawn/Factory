# The Codex plan's quota

Reviews run on Codex, and its plan has two usage windows, a 5-hour one and a weekly one. The daemon's quota keeper keeps both, beside the Claude plan's (see [claude-quota.md](claude-quota.md)) and the reviewer cooldowns, so reviews hold off Codex when either window is nearly spent and go on to the next backend of their chain.

## The signal

`codex app-server` speaks JSON-RPC over stdio, one JSON object a line, with no `jsonrpc` field. The exchange is three messages, sent at once:

1. `{"id":1,"method":"initialize","params":{"clientInfo":{"name":"factory","title":null,"version":"0.1.0"}}}`
2. `{"method":"initialized"}`
3. `{"id":2,"method":"account/rateLimits/read"}`

The server prints notifications of its own between the replies, so lines are read until the one whose `id` is 2 and that carries no `method`. The server runs until it is stopped; the process, with its group, is killed once the answer is in or `[codex] timeout_seconds` (default 20) has passed. The exchange spends no model tokens.

The reply's `result.rateLimits` has `primary` and `secondary` windows, each `usedPercent`, `windowDurationMins` and `resetsAt` (epoch seconds), and the `planType`. A window is the 5-hour one at 300 minutes and the weekly one at 10080, whichever slot carries it; a window of another length, or none, is left out; a 5-hour or weekly window whose share or reset can't be read makes the whole answer unreadable (`its weekly window could not be read`; so does a window that names no length, or one that is no number of minutes). Only a window that is not there, or `null`, is absent, so a hold on it is never dropped for a torn reply. `result.rateLimitResetCredits` gives the reset credits: the rows marked `available`, or its `availableCount` when that is larger.

## How it works

- The keeper asks Codex when its reading is older than 5 minutes, and only when something looks: `factory status`, `factory bar`, `factory quota codex` or a review's dispatch. Looks that find the reading fresh make no call. One ask is out at a time, on a thread of its own: a look that finds it out waits for it for 8 seconds at most, so a hung Codex never makes the bar say the daemon is down. A review that is about to take Codex is the exception: it waits for the ask to end (an ask ends by itself, killed at its timeout, whoever started it), and grants Codex only on a plan so settled, so none starts on a plan that an answer on its way would have held. A review that another backend takes asks nothing.
- A failed ask counts as an ask: a Codex that is down is not pressed again within 5 minutes. The reason shows as `codex usage unreadable: <reason>` (`codex is not installed`, `it exited with status 3 before it answered`, `it did not answer within 20s`, `its answer has no rate limits`, `its answer is not JSON`) in `factory status` and `factory quota codex`, and in the bar as a `?` chip with the reason in the card's `error`: muted, unless a standing reading still holds dispatch, when the chip is critical and shows that hold. An unreadable Codex is never 0%, and a failed ask lifts nothing: a hold that a standing reading still names is kept beside the reason.
- Dispatch holds once `[codex] hold_at_percent` (default 90) of either window is spent, until that window resets. The mark compares the share as Codex gives it, as Claude's does, and a share is shown rounded down to a whole percent everywhere: 89.5% shows as 89% and does not hold at 90, 90.0% shows as 90% and holds. The weekly window is as good a reason as the 5-hour one.
- A reading whose window has reset since is unknown, not 0%, and holds nothing.
- A review's chain steps over a held backend: a backend that runs on Codex cools down until the hold ends, so the router grants the next one. Every pick lays the hold that the stored reading names over Codex, so a held Codex is stepped over even while its slots are full; the pick that falls to Codex on a stale reading starts the ask under the same lock and waits for it before it grants. With none left the review exits 75 (`every review backend is out of quota (codex until <time>, held at weekly 92%); retry after <time>`). A hold read more than 5 minutes ago is asked again before it refuses or holds back a review, and a review that waits on a hold looks again at the plan every 5 minutes. The pick that grants is made from the reading as it stands under the router's lock, so an answer that lands from another look's ask between the two looks is the one obeyed. The hold is no stored cooldown: it lifts the moment the plan's windows do.
- `factory quota codex` exits 0 to dispatch (`codex: 5h 18% resets …, weekly 40% resets …; plan plus, 2 reset credits; dispatch`), or 75 naming what holds (`codex: hold: weekly 92%; hold new reviews on codex until <time>; …`).
- `factory status` shows one `codex:` row, and the review quota's Codex backends say `held at weekly 92%` under their row while it holds. `--json` shows a `codex` object with each window's reading, the plan and `reset_credits`.
- `factory bar` prints `claude 12/40 · codex 18/92 · agy 30/63 · 2 leads · lab 2/2`. `factory bar --json` has a Codex chip of the worst window (critical while held) and a card with a meter per window, the plan and the reset credits available.
- The daemon's database keeps the last reading (table `codex_quota`, no schema bump: it is created with the rest, and a table an earlier build made without `asked_ms` gains it, set to the last window it saw), with the time it was asked, read or not, so a restart shows the last windows at once and, within 5 minutes of the ask, makes no new call even after a failed one. Whether the last ask could be read is kept in memory only. A reading that could not be written shows as `codex usage unreadable: its reading could not be kept: <reason>; check the disk and FACTORY_STATE` beside the reading it still holds on; each look tries to write it again, and the line goes once it has been.

## Config

```toml
[codex]
hold_at_percent = 90   # more than 0, at most 100
timeout_seconds = 20   # 1 to 20: how long app-server may take before it is killed; more is refused
```

Each reading joins the usage history (harness `codex`, no quota group) through the keeper's shared store path, as Claude's do: `factory quota history` shows it (see [claude-quota.md](claude-quota.md)). A look that asks nothing adds nothing, nor does a failed ask: the windows it keeps on are no new reading, and are never written back into the history once pruned.
