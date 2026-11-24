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

So its sandbox mounts the workspace (the working directory and each `--add-dir`) read-only and leaves the rest of the filesystem writable. A review can not change the repo it reviews. The rest of the filesystem is closed by the write guard below (#49). The argument vector that gives this (`--mode plan`, `--sandbox`, `--add-dir`) is pinned by the tests in `crates/factory/tests/cli/agy.rs`, and a review whose own log doesn't say the sandbox started is not kept.

Limits of the finding: one run, on the default Gemini model, on Linux with unprivileged user namespaces. A write outside the workspace still works, which is why the checkout is thrown away and is never the caller's.

## agy-claude, live

`agy-claude` (agy on a Claude model) had its first live run since the port on 2026-11-24: `factory review` with one backend `cli = "agy"`, `model = "claude-sonnet-5-5-low"`, on a two-commit scratch repo. It reviewed, wrote a report with a findings ledger, and exited 10 on its one finding. Its quota group (Claude and GPT models) had room.

## Write guard (#49)

agy's own sandbox leaves everything outside its workspace writable, so `factory review` starts agy inside bubblewrap: `bwrap <guard> -- agy <the review's arguments>`. The guard is built in `crates/factory-adapters/src/guard.rs`.

What a guarded review can write:

- its throwaway checkout, the one it runs in (`--bind`)
- its own log, `reviews/<repo>/<label>.agy.run.log` (`--bind`)
- agy's state: `~/.gemini`, `${XDG_CONFIG_HOME:-~/.config}/antigravity`, `${XDG_CACHE_HOME:-~/.cache}/antigravity` (`--bind-try`, so a missing dir is skipped)
- a private `/dev/shm` and an empty `$XDG_RUNTIME_DIR`, both tmpfs, thrown away with the review

What it refuses:

- every other path, `$HOME`, other repos, the shared git dir and `/tmp` included, because the root is mounted read-only (`--ro-bind / /`). `/tmp` is left read-only rather than made a tmpfs, so inputs that live there stay visible.
- the sockets in the runtime dir (the factory daemon's, herdr's and others). A socket on a read-only mount still connects, so the runtime dir is replaced by an empty tmpfs. Only the session bus is bound back in, read-only, because agy's login reads its token from the Secret Service keyring over that bus.
- Docker, whose socket `/run/docker.sock` is masked with `/dev/null`

Reads are unchanged: the whole filesystem stays readable, so git history, the bundle, the spec, the evidence and the previous report read as before. A fresh `/proc` is mounted so that agy's own sandbox can start its user namespace inside the guard. There is no pid namespace, so the review's process group is still killed on a timeout. Each path is bound at its real location, with symlinks followed, because bwrap can't mount on a symlink.

Before an agy review starts, the backend's probe runs `unshare --user --map-root-user true` bare and then inside the guard. If either fails (no unprivileged user namespaces, or no bwrap), agy is skipped with the probe's first line as the reason, and the chain moves on. If no other backend can take the review, `factory review` exits 3 with one line. The tests in `crates/factory/tests/cli/agy.rs` pin the guard's arguments, the skip, and the exit 3, with fake `bwrap` and `agy` on PATH.

### Live run (2026-11-24, agy 1.5.15, bubblewrap 0.13.1)

The built `factory review` was run with one backend, `cli = "agy"`, on a scratch repo outside `/tmp`. The run had a spec, an evidence file and the previous round's report. The spec asked the reviewer to run `touch ~/t49-live-home` and `touch ~/t49-other-repo/t49-live-other` (the second is a different git repo) in its terminal and quote the output. The report says:

```
touch: cannot touch '~/t49-live-home': Read-only file system
touch: cannot touch '~/t49-other-repo/t49-live-other': No such file or directory
```

Both exited 1, and neither file exists afterwards. The second message is agy's own sandbox, which hides paths outside its workspace. Under it, the guard keeps that repo read-only. The same review judged the fix commits against round 1's findings (2 fixed, 1 still present), cited the evidence and the spec, and exited 10 on its findings, as before.

The first live attempt found a bug: the guard failed to start when an agy state dir was a symlink (`bwrap: Can't mount on symlink destination`). agy was skipped with that reason, which is how a guard that can't start is meant to fail. Bound paths now follow symlinks.
