# Factory

A Rust harness for an agent software factory: Claude Code agents build, a cross-vendor reviewer reviews, and one daemon does the waiting, polling and bookkeeping so the agents don't spend tokens on it.

One binary, `factory`:

- `factory lab` (also answers to `devlab`): leases phones, emulators and local stack slots between agents.
- `factory review`: routes a code review to the first reviewer with quota left (Antigravity, then Codex, then Claude).
- `factory wait ci|review|lead|lease`: blocks on the daemon, silently, until a PR's checks finish, a review ends, a lead finishes or needs attention, or a lab lease is held no more, then prints one line. `wait ci` and `wait lead` take `--notify`: the command returns at once, and the daemon posts the line into the asking Claude Code session when the wait ends. That event delivery stays off until `[notify] enabled = true` in the config; [docs/notify-night.md](docs/notify-night.md) is the supervised-night checklist.
- `factory cutover`: hands the `devlab` name from today's TypeScript devlab to factory lab, and `--rollback` gives it back ([docs/cutover.md](docs/cutover.md)).
- `factory lead`, `factory monitor`, `factory log`, `factory hook`, `factory status`.

`factory hook pre-tool|post-tool|subagent-stop` answers Claude Code's hooks in about 2 ms, without the daemon. Wire them in `~/.claude/settings.json` (`PreToolUse` on `Bash|REPL`, `PostToolUse`, `SubagentStop`, each a `command` hook running `factory hook <event>`). They block npm, npx, yarn and pnpm (`FACTORY_ALLOW_NPM=1` is the escape), `pkill -f` and `killall` (no escape: they reach every agent's processes), a commit whose staged diff adds an address from `[hook] private_addresses` or a push sending a commit that does, a REPL script using `process`, `require` or `BASH_REMATCH`, a CPU burner outside a `systemd-run -p CPUQuota=` scope (`yes >/dev/null`, an empty endless loop, `stress`, `openssl speed`), an `rm` on a variable, glob or `$(…)` or inside `bash -c`, a `$` inside a quoted grep or rg pattern, a command that starts with `[object`, and a REPL script that runs `factory wait` from a loop, a callback or a timer, or twice (the shell rules read each REPL `$` template too); they add context to a REPL `git push` whose `.timeout` is under 590000 ms, note a tool result over 20k characters to the model as context (not as a blocking error), and send back a subagent report longer than its `[hook.report]` template. A hook prints nothing to let a call through, and one protocol JSON line otherwise; a commit or push in a repository it cannot work out (`cd "$REPO"`, say) exits 2 with one line on stderr, which blocks it. So it keeps to its own protocol, not the one-line rule below.

Every command prints one line by default, `--json` on request, and uses fixed exit codes.

Status: every command above works through the daemon, which `factory install` sets up as a systemd user socket. `factory review` tries its backends in the order `~/.config/factory/config.toml` gives them. The spec is issue #1.

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT licence](LICENSE-MIT), at your option.
