# Cutting devlab over to factory lab (#20)

`factory cutover` hands the `devlab` name on PATH from today's TypeScript devlab to factory lab,
in one command, and `factory cutover --rollback` gives it back.

```
factory cutover [--json]                     # devlab becomes factory lab; the TS watcher stops
factory cutover --rollback [--force] [--json] # devlab is today's devlab again; its watcher starts (not with --force and no unit)
```

## Before you cut over

- DotFiles' cutover refusal (`refuseIfCutOver` in `devlab/`) is in the checkout today's devlab
  runs from. Without it the cutover refuses at once and changes nothing.
- `factory install` has put `factory` in `~/.local/bin` and the daemon runs, so factory lab
  answers.
- `devlab status` shows no lease and nobody waiting.

## What the cutover does

1. It refuses with exit 75, in one line, while `devlab status --json` shows a lease or a waiting
   request: `factory: devlab status shows lease 7 demo-a (claude), …; cut over once devlab status shows no lease`.
2. It takes today's devlab's write lock (`devlab.db`, IMMEDIATE), checks the ledger for a
   lease or waiter that status did not show, and writes the marker
   `$XDG_STATE_HOME/devlab/cutover.json` (or `~/.local/state/devlab/cutover.json`) under that lock.
   From then on today's devlab refuses every call but `help`, with exit 3 and
   `this devlab was cut over to factory lab; …`. A call already under way meets the marker at
   its next transaction, so the two never write one database.
3. It checks that today's devlab now refuses and that factory lab answers.
4. It disables and stops `devlab-watch.service`, when its unit is in `~/.config/systemd/user`,
   and keeps the unit file. Stow links that file into the devlab package, and systemd's
   `disable` deletes a unit file that is a link along with its `[Install]` link (`is-enabled`
   then says `not-found`). So the cutover records the link in the marker first and puts it back
   straight after `disable`: only the `[Install]` link stays gone, so the watcher does not start
   at login, and `is-enabled` says `disabled`.
5. It points `~/.local/bin/devlab` at `factory`, through a rename, so the name is never missing.

A step that fails undoes the ones before it: the watcher starts again, the marker goes, and the
link is left as it was. A second `factory cutover` says `already cut over` and changes nothing.

DotFiles ignores `.local/bin/devlab` in `devlab/.stow-local-ignore`, so `dots link` leaves the
cutover's link alone.

Afterwards `GOLDEN_DEVLAB=$HOME/.local/bin/devlab cargo test --test cli transcript::` replays the
golden transcripts against what is on PATH.

## Rolling back

`factory cutover --rollback` refuses with exit 75 while factory lab holds a lease, a waiting
request or a reservation. Otherwise it puts the old link back (the marker names it), checks
factory lab again, starts `devlab-watch.service` (`enable --now`) and removes the marker.
When the watcher's unit link went missing after the cutover (by hand, or a `dots unlink`), the
rollback links it again from the path the marker records before `enable --now`. A marker from
before #30 records no unit, so a missing unit with such a marker makes the rollback refuse in
one line, changing nothing: link the unit again with `dots link`, then run
`factory cutover --rollback` again.
`--force` goes on instead when the unit is missing and cannot be linked again (the marker records
no link, or the link cannot be made), or when it is a link to nothing: devlab and the marker are
put right, `enable --now` is not called, one line names the unit it skipped (`"watcher": "skipped"`
and `"skipped"` in `--json`), and the watcher stays off until you link the unit and enable it.
Where the unit can be linked again, `--force` does that as above. A unit that is there and
that systemd will not start stops the rollback, `--force` or not, with the marker kept: fix
systemd, then run the rollback again.
`--force` also skips the check that factory lab holds nothing, for a factory lab that no longer
answers. A second rollback says `not cut over` and changes nothing.

Leases held in factory lab's `lab.db` don't carry back: roll back when nothing is leased.
