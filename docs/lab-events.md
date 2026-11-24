# Lab events in the stats ledger

The daemon writes what the lab does to `events.jsonl` (#62), one line per fact, as it happens:
the same ledger and line format as `factory log`.

- Every event is queued in `lab.db`: lease starts and ends, reclaims, readiness changes and
  gave-up waits by the transaction that makes them true, so a grant that is rolled back is
  never told.
- A granted wait is queued once its caller says it has the grant: one that fails as it is
  settled (an emulator that won't boot) or never reaches its caller was no grant, and its lease
  ends `reason=settle-failed`. A grant handed over by a daemon after a restart counts its wait
  until then.
- The ledger's own thread, on its own connection, appends the queue and then forgets it, so no
  lab call waits on the ledger, and a backlog a stopped daemon left goes out from the next one
  (a line appended just before a stop may go out twice). `lab-run` takes the same path.
- A ledger that can't be written, or opened for writing, costs one daemon log line per event
  and changes no lab command's exit code or output. One that can't even be read doesn't stop
  the daemon either: the leads a stopped daemon left, which read their standing from it, cost
  one log line and are settled once it can be read.

Values are flat. Numbers are only in fields on the numeric list (`NUMERIC_FIELDS` in
`crates/factory-core/src/event.rs`). No value names a device serial, a home path or a user: a
worktree is its basename (`~` when it is named as the user or their home dir, from the
environment or `/etc/passwd`), a phone is its model
plus `phone`, the first 8 hex digits of its serial's sha256, and a wait's reason keeps its kind
and counts, never its text.

| type | fields |
| --- | --- |
| `lab-lease` | `action=start|end`, `lease`, `project`, `owner` (label), `worktree` (basename), `devices` (`kind:model,…`), `count`, `slot`, `reservation`; on `end` also `seconds`, `idle_seconds` (no exec or run running), `reason=run-end|release|owner-exit|reclaim|settle-failed` |
| `lab-wait` | `action=granted|gave-up`, `waiter`, `asked=phone|emulator|any|none`, `count`, `stack=yes|no`, `seconds`, `wait` (the last reason's kind: `none`, `phones`, `emulator-cap`, `device-cap`, `slot-cap`, `memory`, `queue`, `slots`, `adb`, `reprobe`, `other`), and its numbers: `busy`, `locked`, `asleep`, `stay_awake_off`, `offline`, `unauthorized`, `cap` |
| `lab-reclaim` | `lease`, `kind`, `model`, `idle_minutes`. No `waiter`: the device is free once taken, and the waiter that gets it writes its own `lab-wait action=granted` |
| `lab-readiness` | `model`, `phone` (short id), `from` (`none` when first seen), `to`, `notified=true|false` (the change earned a notification and notifications are on; whether notify-send then delivered it is in the daemon's log, and an offline held back 3 s that its unplug followed is said as the unplug alone) |

The last reason a request waited for is kept in `lab.db` until the request is gone, so a wait
that runs out across a daemon restart, or a grant handed over after one, still says what it
waited for. A lease granted before its clock was kept counts
from when it was made, and the commands run on it from the first one this daemon saw.

A reclaim takes a device from a lease and leaves the lease held, so `lab-reclaim` is the record
of the device leaving. No path ends a lease by reclaim today: `reason=reclaim` is reserved.
