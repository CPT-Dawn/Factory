# Factory

A Rust harness for an agent software factory: Claude Code agents build, a cross-vendor reviewer reviews, and one daemon does the waiting, polling and bookkeeping so the agents don't spend tokens on it.

One binary, `factory`:

- `factory lab` (also answers to `devlab`): leases phones, emulators and local stack slots between agents.
- `factory review`: routes a code review to the first reviewer with quota left (Antigravity, then Codex, then Claude).
- `factory wait`: blocks on CI, a review, a lead or a lease, silently, then prints one line. With `--notify` it returns at once, and the daemon posts the line into the asking Claude Code session when the wait ends; that event delivery stays off until `[notify] enabled = true` in the config.
- `factory cutover`: hands the `devlab` name from today's TypeScript devlab to factory lab, and `--rollback` gives it back ([docs/cutover.md](docs/cutover.md)).
- `factory lead`, `factory monitor`, `factory log`, `factory hook`, `factory status`.

`factory hook pre-tool|post-tool|subagent-stop` answers Claude Code's hooks in about 2 ms, without the daemon. Wire them in `~/.claude/settings.json` (`PreToolUse` on `Bash|REPL`, `PostToolUse`, `SubagentStop`, each a `command` hook running `factory hook <event>`). They block npm, npx, yarn and pnpm (`FACTORY_ALLOW_NPM=1` is the escape), a commit or push adding an address from `[hook] private_addresses`, and a REPL script using `process`, `require` or `BASH_REMATCH`; they flag a tool result over 12k characters, and send back a subagent report longer than its `[hook.report]` template. A hook prints nothing to let a call through, and one protocol JSON line otherwise, so it keeps to its own protocol, not the one-line rule below.

Every command prints one line by default, `--json` on request, and uses fixed exit codes.

Status: being built; `factory log` works, through the daemon, and so does `factory review` on Codex, falling back to Claude, with its backends in `~/.config/factory/config.toml`. The spec is issue #1.

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT licence](LICENSE-MIT), at your option.
