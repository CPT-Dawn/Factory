# The factory measures, the bar only displays

Every measurement the user sees at a glance (each harness's quota, the lab, CI and the PeekaBus runner fleet) is taken, cached and kept as history by the factory daemon. The status bar (Zenith) only renders the bar feed that `factory bar --json` prints, and holds no data, credentials or polling of its own. The factory is the thing being measured and improved, so its data has to live where factory-stats can compare nights, leads and quota burn, and one daemon polling each source is cheaper than a bar and a feed script polling the same ones.

## Considered Options

- **Zenith modules poll their sources directly** (its runners module calling GitHub with a `gh` token). Rejected: the data dies in the bar, can't be measured, and duplicates the daemon's GitHub polling.
- **A separate feed script** (the bun `cockpit-feed` in DotFiles). Rejected: a second keeper of caches and parsers that drifts from the factory's, and a bun start on every tick.

## Consequences

- Zenith's runners module stays in its public release but is left unconfigured on this PC; don't re-enable it here.
- When the daemon is down, `factory bar --json` still prints a valid feed that says so, and exits 0.
