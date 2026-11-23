# Coding standards

What review checks that tooling does not. The rules under "Enforced by tooling" fail CI on their own, so review skips them.

## Seams

- `factory-core` stays pure. Time, randomness, files and processes come in as arguments or traits. A planner or rule that reads the clock or spawns a process is a finding.
- External tools are reached only through their adapter traits in `factory-adapters`, and each trait has a fake.
- Tests sit at two seams:
  - The binary: argv in; stdout, stderr and exit code out. State is observed through another command.
  - The core's pure functions: table and property tests.

  A test that reaches into the daemon's tables or private types is a finding.
- A test waits on observable state with a deadline. A fixed sleep in a test is a finding.

## Behaviour

- The output contract (AGENTS.md) holds for every command. A new exit code or a second line of output is a finding unless the spec names it.
- Every external process has a timeout and runs in its own process group, which is killed whole on timeout.
- No external command runs while a SQLite write transaction is open.
- The CLI waits by sending the daemon a request that is answered when the event fires. A sleep-and-poll loop in the CLI is a finding. Polling GitHub from the daemon uses conditional requests.
- A daemon restart loses no lease, wait or cooldown: that state lives in SQLite. Lease ids are monotonic and never reused.
- An error is one line the agent can act on: what failed and what to do next.

## Enforced by tooling

- Formatting: `cargo fmt --check`.
- Lints: `cargo clippy --all-targets -- -D warnings`, with the workspace lints in the root `Cargo.toml`.
