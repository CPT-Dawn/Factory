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
- A label with no `-rN` suffix (`t1`) finds no previous round, and runs only as `--standalone` (amended below).

## Amendment, 2026-11-29: no way around the cap (Factory#85)

After the cap went live, leads went around it: a new prefix on the refused diff (`t182fix-ece1f305-r1` after
`t182-r5-light` was refused, `t511fix-845b-r1` after `t511-r5`), a label the cap could not parse
(`t434-r5-light-2`, `retro-lab-light2`), which it held to no cap, and `--escalation` on rounds no lead had
escalated. So `factory review` now refuses, each time in one line that says what to do instead:

- **A label that names no round**, unless the review is `--standalone` (a review outside a ticket, such as a
  retro's). `--standalone` with a `-rN` label, or with `--escalation`, is refused.
- **`--escalation` without the lead's word.** The lead that re-dispatches a ticket logs
  `factory log escalation repo=<repo> prefix=<prefix>`; rounds 3 and 4 need that event in the ledger.
- **A fresh review of capped work.** A prefix has used its cap once it ran its light review, its round 4, or its
  round 2 with no escalation event. A new round 1 (any prefix) or a standalone review of work such a prefix
  reviewed is refused: the line says to file a follow-up issue for what is still open and review it with
  `--follow-up <issue>`, which must name an issue gh can view, other than the spec. The same work is the same
  head, or the same tree reviewed from the same base, so an empty commit does not make it new; a diff moved onto
  another base is not caught. The follow-up issue lands in the `review` event as `follow_up`.

The ledger is the record because it is what the daemon already keeps per repo and what factory-stats reads; the
reviews directory alone would forget a review whose files were removed. Rejected: checking that a follow-up issue
is younger than the capped review (one more gh field for a rule the owner reads in the event anyway).
