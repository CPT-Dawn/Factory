# agy's sandbox and the shared git dir

`factory review` runs agy on a throwaway detached checkout of the head and gives it the repo's shared git dir with `--add-dir`, so it can read history. This page records whether agy's terminal can write to that git dir.

## Finding (2026-11-24, agy live run)

It can not. agy was run as the reviewer runs it (`-p`, `--mode plan`, `--sandbox`, `--add-dir <repo>/.git`, `toolPermission: proceed-in-sandbox`, from a detached worktree of that repo) and told to run four `touch` commands in its terminal:

| Target | Result |
|---|---|
| a file in the `--add-dir` git dir | `Read-only file system` |
| a file in that git dir's `worktrees/` | `Read-only file system` |
| a file in its own working directory, the checkout | `Read-only file system` |
| a file in a directory that is neither | succeeded |

So its sandbox mounts the workspace (the working directory and each `--add-dir`) read-only and leaves the rest of the filesystem writable. A review can not change the repo it reviews, so no further guard was added. The argument vector that gives this (`--mode plan`, `--sandbox`, `--add-dir`) is pinned by the tests in `crates/factory/tests/cli/agy.rs`, and a review whose own log doesn't say the sandbox started is not kept.

Limits of the finding: one run, on the default Gemini model, on Linux with unprivileged user namespaces. A write outside the workspace still works, which is why the checkout is thrown away and is never the caller's.

## agy-claude, live

`agy-claude` (agy on a Claude model) had its first live run since the port on 2026-11-24: `factory review` with one backend `cli = "agy"`, `model = "claude-sonnet-5-5-low"`, on a two-commit scratch repo. It reviewed, wrote a report with a findings ledger, and exited 10 on its one finding. Its quota group (Claude and GPT models) had room.
