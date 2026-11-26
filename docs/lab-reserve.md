# Reserving stack slots for implementers (#17)

`devlab reserve` (also `factory lab reserve`) lets a lead set stack slots aside for the
implementers it is about to dispatch, so an implementer never queues for a slot inside its own
context.

```
devlab reserve [--slots N] [--wait SEC] [--json]   # grant up to the owner's cap
devlab reserve --release [--json]                  # give this owner's reservations back
```

- `reserve` grants as many of the `--slots` (default 1) as the owner's per-owner cap leaves room
  for, and prints how many it granted and their ids:
  `granted 1 of 2 demo slots (cap 1): reservations 7; pass each to an implementer as DEVLAB_RESERVATION=<id>`.
- A reservation counts against the cap like a lease, and shows in `devlab status` as
  `reserved by <owner> (reservation <id>)`.
- The lead gives each implementer one id in its environment, `DEVLAB_RESERVATION=<id>`. The
  implementer's `devlab acquire` (or `run`) turns the reservation into its own lease at once, for
  its own worktree, with no turn in the queue. A reservation that is gone (used, released, or its
  lead exited) is ignored: the acquire queues the normal way.
- An owner with no room left under its cap gets exit 75 at once, not a wait:
  `devlab: owner <label> already holds 1 of 2 demo stack slots (cap 1); a slot of its own frees in ~30 min: release a lease or reservation, or retry then`.
  The ETA is the median of how long the project's last leases ran (30 min before any has), less how
  long the owner's oldest holding has run, never under a minute.
- When no slot is free the request waits (`--wait`, default as for acquire) and prints exactly two
  lines in total: `devlab: waiting: <why>` when it starts to wait, and its answer.
- Reservations live in the daemon's `lab.db`, so a daemon restart keeps them. They go when their
  owner exits, and when no lease has used one for 20 minutes since it was granted or its last
  lease ended (or 2 h, when no call reached the daemon meanwhile): `devlab gc` says which and why,
  and `devlab status` shows each one's `lapses in N min unless used`.

## Which stacks the bar's Lab card shows

The stack sweep looks at every slot of each project the lab knows: one with a slot the store
records, a lease or a reservation, or one whose `devlab.json` a worktree that held or reserved
a slot names (all of its `maxSlots`, so a stack the owner started by hand on a slot no lease
took shows as theirs). A project the lab has never seen, with no lease, reservation or slot
record, is out of its reach: its stacks are not on the card, since nothing names the project
or its slot ids without asking docker for every container on the machine.

## The matching change for the harness repo (implement-spec)

Not made here; the harness repo is edited by its owner.

1. Where implement-spec dispatches an implementer for a ticket that needs a stack, the lead runs
   `devlab reserve --slots <n>` first, with n the number of implementers it runs at once.
2. It passes one printed id to each implementer as `DEVLAB_RESERVATION=<id>` in the implementer's
   environment; the implementer's own `devlab acquire` then takes the reserved slot.
3. An implementer dispatched without an id falls back to a plain `devlab acquire`, and so can
   still wait for a slot when its lead is at its cap.
4. On exit 75 from `reserve` the lead dispatches fewer implementers (or waits for one to finish);
   it never retries in a loop.
5. When the implementers are done, `devlab reserve --release` frees any reservation left unused.
