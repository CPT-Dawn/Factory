# Factory

One Rust binary, `factory`, and the daemon it runs, for an agent software factory on one Linux workstation. The spec is issue #1. Its tickets are sub-issues with native blocked-by edges.

- Layout: a Cargo workspace of three crates. `crates/factory-core` is the pure domain: no I/O, no clock, no processes. `crates/factory-adapters` puts each external tool behind a trait that has a fake. `crates/factory` is the CLI, the daemon and the `devlab` multicall. Spec #1, "Workspace of three crates".
- Every command keeps the **output contract**: one line on stdout by default (about 200 characters, naming a file when there is more), silent while waiting, `--json` everywhere, and exit codes from the one table in the code. Spec #1, "Output contract".
- **Parity**: invoked as `devlab`, the binary matches the golden transcripts (#13) byte for byte in stdout, stderr and exit code. Error text that factory-stats' classifier matches stays exactly as it is. A change to a golden file is a behaviour change: the commit says so.

## Done

- The gate is `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`, as CI runs it.
- The pre-push hook in `.githooks` runs the gate before every push of the checked-out branch (a push that only deletes branches skips it); enable it with `git config core.hooksPath .githooks`.
- Tests drive the built binary with a temp state and runtime directory, and with scripted fakes of adb, docker, systemctl, gh, herdr, agy, codex and claude on PATH. They need no devlab lease, phone or stack. Only #19 and #20 work on the real machine.

## Public repo

- Paths in code, tests, docs and commits use `$HOME`, `~` or a temp dir. Real home directories, usernames, hostnames and device serials stay out of the repo.

## Agent skills

- Issue tracker: GitHub issues on `CPT-Dawn/Factory` through `gh`. Sub-issues and blocking are described in `docs/agents/issue-tracker.md`.
- Triage labels: the five canonical names, unchanged (`docs/agents/triage-labels.md`).
- Domain docs: single context, with `GLOSSARY.md` and `docs/adr/` at the root (`docs/agents/domain.md`).
