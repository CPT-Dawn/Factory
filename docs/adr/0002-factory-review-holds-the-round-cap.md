# factory review holds the round cap and finds the round before

`factory review` decides which round a label may run and what that round continues from; callers no longer can. For a label `<prefix>-rN` with N > 1 it reads the round before (`<prefix>-r<N-1>` in the same repo's reviews: its report, its findings ledger and the head it reviewed) unless the caller names a previous report, and refuses the round when that report is missing. Rounds 1 and 2 always run; rounds 3 and 4 only with `--escalation`, which implement-spec's Opus re-dispatch passes; after the last round (2, or 4 for an escalation) one light review, `<prefix>-rN-light`, reviews only the last fix from round N's head. Anything past that is refused with one line telling the agent to report what is open. A round whose previous head is not on its branch (an escalation that starts a new branch) reviews the whole diff but still carries the previous round's open findings, so its ledger marks them `fixed` or `still_present` instead of starting fresh.

The 2026-11-27 harness retro found the cap and the fix-round scoping were the callers' to keep, and they didn't: one implementer ran eight rounds of one ticket, rounds 2 to 8 each re-reviewing the whole branch with no ledger, so every round found new blockers, and 82% of round-2 reviews still had findings.

## Considered Options

- **Keep the cap in the skills' text** (`code-review` says "two rounds is the cap"). Rejected: that is what failed; a caller that leaves out `<previous-report>` gets a fresh review in silence, and nothing stops round 3.
- **Refuse any round past 2, escalations included.** Rejected: implement-spec's escalation to Opus needs its own two rounds to show whether the stronger model settled the findings.
- **A `--light` flag.** Rejected for the label suffix: the label already names the round and the report file, and review.sh passes labels through unchanged.

## Consequences

- review.sh must pass `--escalation` through to `factory review`; until it does, an escalation's round 3 is refused.
- A light review's event has `round` N + 1 and a label ending `-light`; factory-stats tells it from an escalation's round 3 by the label.
- A label with no `-rN` suffix (`t1`) is held to no cap and finds no previous round, as before.
