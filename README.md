# Factory

A Rust harness for an agent software factory: Claude Code agents build, a cross-vendor reviewer reviews, and one daemon does the waiting, polling and bookkeeping so the agents don't spend tokens on it.

One binary, `factory`:

- `factory lab` (also answers to `devlab`): leases phones, emulators and local stack slots between agents.
- `factory review`: routes a code review to the first reviewer with quota left (Antigravity, then Codex, then Claude).
- `factory wait`: blocks on CI, a review, a lead or a lease, silently, then prints one line.
- `factory lead`, `factory monitor`, `factory log`, `factory hook`, `factory status`.

Every command prints one line by default, `--json` on request, and uses fixed exit codes.

Status: specified, not built yet. The spec is issue #1.

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT licence](LICENSE-MIT), at your option.
