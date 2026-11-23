# The Claude plan's quota

At 08:30 IST on 2026-11-23 the plan's 5-hour limit (HTTP 429) ended six subagents mid-ticket. The daemon now keeps a quota for the Claude plan, beside the reviewer quotas, so a lead holds new dispatches when the window is nearly spent and resumes once it resets.

## Signals, and the one chosen

| Signal | Documented | Gives | Used |
|---|---|---|---|
| Statusline stdin JSON, `rate_limits.five_hour.used_percentage` and `resets_at` (epoch seconds) | yes: [statusline](https://code.claude.com/docs/en/statusline) | share spent and reset, Pro/Max only, after a session's first answer | **primary** |
| `StopFailure` hook, `error: "rate_limit"` and `last_assistant_message` | yes: [hooks](https://code.claude.com/docs/en/hooks) | that a turn hit a limit; the reset only as message text | **fallback** |
| Limit messages: `You've hit your session limit · resets 3:45pm`, `... weekly limit · resets Mon 12:00am` | yes: [errors](https://code.claude.com/docs/en/errors) | a local wall-clock reset with no date or zone | parsed by the fallback |
| `/usage` | interactive only: [commands](https://code.claude.com/docs/en/commands) | nothing a daemon can read | no |
| `anthropic-ratelimit-unified-*` response headers | no | n/a | no |
| Transcript JSONL fields | no schema: [sessions](https://code.claude.com/docs/en/sessions) | the error text | no |

The statusline is the only documented signal that gives both the share spent and the reset. The hook catches a 429 that arrives before any statusline has run, or on a machine without one.

## How it works

- `factory statusline` is Claude Code's `statusLine` command. It reads the statusline JSON, sends the 5-hour window to the daemon, and prints `<model> · 5h <n>% · resets HH:MM`. A daemon that doesn't answer costs the line nothing.
- `factory hook stop-failure` is the `StopFailure` hook. A turn that ended on a rate limit records a limit until the reset its message names. Without a named reset, the limit lasts until the window's known reset, else one hour. A limit that is already holding keeps its reset unless a later one is named. Other errors change nothing.
- A statusline window with room (under 100%), seen after a limit was recorded, lifts the limit, because that 429 was a capacity limit and not the plan's.
- `factory quota claude` exits 0 to dispatch, or 75 with `hold new dispatches until <time>`. It holds once `[claude] hold_at_percent` (default 90) of the window is spent, or while a limit holds.
- `factory wait claude [--timeout 9m]` blocks, silent, until a lead may dispatch (exit 0), or exits 124 to wait again.
- `factory status` shows one `claude:` row, and `--json` shows a `claude` object.
- The daemon's database keeps the quota (table `claude_quota`, schema 4), so a restart forgets no hold.

## Wiring (DotFiles)

`~/.claude/settings.json`:

```json
"statusLine": {"type": "command", "command": "factory statusline"},
"hooks": {"StopFailure": [{"matcher": "rate_limit", "hooks": [{"type": "command", "command": "factory hook stop-failure"}]}]}
```

implement-spec runs `factory quota claude` before each dispatch. On exit 75 it runs `factory wait claude` until that exits 0, then dispatches.
