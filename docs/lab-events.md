# Lab events in the stats ledger

The daemon writes what the lab does to `events.jsonl` (#62), one line per fact, as it happens:
the same ledger and line format as `factory log`.

- Lease starts and ends, reclaims, readiness changes and gave-up waits are queued in `lab.db`
  by the transaction that makes them true, so a grant that is rolled back is never told.
- A granted wait is written once its grant is handed over: a grant that fails as it is settled
  (an emulator that won't boot) was no grant, and its lease ends `reason=settle-failed`.
- One thread appends them, so no lab call waits on the ledger. A ledger that can't be written,
  or even opened, costs one daemon log line per event; the daemon starts all the same, and no
  lab command changes its exit code or output.

Values are flat. Numbers are only in fields on the numeric list (`NUMERIC_FIELDS` in
`crates/factory-core/src/event.rs`). No value names a device serial, a home path or a user: a
worktree is its basename (`~` for the home dir or a dir named like it), a phone is its model
plus `phone`, the first 8 hex digits of its serial's sha256, and a wait's reason keeps its kind
and counts, never its text.

| type | fields |
| --- | --- |
| `lab-lease` | `action=start|end`, `lease`, `project`, `owner` (label), `worktree` (basename), `devices` (`kind:model,…`), `count`, `slot`, `reservation`; on `end` also `seconds`, `idle_seconds` (no exec or run running), `reason=run-end|release|owner-exit|reclaim|settle-failed` |
| `lab-wait` | `action=granted|gave-up`, `waiter`, `asked=phone|emulator|any|none`, `count`, `stack=yes|no`, `seconds`, `wait` (the last reason's kind: `none`, `phones`, `emulator-cap`, `device-cap`, `slot-cap`, `memory`, `queue`, `slots`, `adb`, `reprobe`, `other`), and its numbers: `busy`, `locked`, `asleep`, `stay_awake_off`, `offline`, `unauthorized`, `cap` |
| `lab-reclaim` | `lease`, `kind`, `model`, `idle_minutes`, `waiter` (the oldest waiter short of that kind, whom the queue serves first: join it to that waiter's `lab-wait`) |
| `lab-readiness` | `model`, `phone` (short id), `from` (`none` when first seen), `to`, `notified=true|false` (the change earned a notification and notifications are on) |

The last reason a request waited for is kept in `lab.db`, so a wait that runs out across a
daemon restart still says what it waited for. A lease granted before its clock was kept counts
from when it was made, and the commands run on it from the first one this daemon saw.

A reclaim takes a device from a lease and leaves the lease held, so `lab-reclaim` is the record
of the device leaving. No path ends a lease by reclaim today: `reason=reclaim` is reserved.
