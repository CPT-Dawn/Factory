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

agy's own sandbox leaves everything outside its workspace writable, so `factory review` starts agy inside bubblewrap: `bwrap <guard> -- agy <the review's arguments>`. The guard is built in `crates/factory-adapters/src/guard.rs`. It needs `bwrap`, and for the subscription login also `xdg-dbus-proxy` (Arch: `bubblewrap` and `xdg-dbus-proxy`).

What a guarded review can write:

- its throwaway checkout, the one it runs in (`--bind`)
- its own log, `reviews/<repo>/<label>.agy.run.log` (`--bind`)
- agy's state: `~/.gemini`, `${XDG_CONFIG_HOME:-~/.config}/antigravity`, `${XDG_CACHE_HOME:-~/.cache}/antigravity` (`--bind-try`, so a missing dir is skipped)
- tmpfs mounts that are thrown away with the review: an empty `$XDG_RUNTIME_DIR`, and a private `/dev` (`--dev`: `null`, `zero`, `random`, `urandom`, `tty` and its own `pts`) with its own `/dev/shm`
- with the subscription login, the Secret Service keyring (`org.freedesktop.secrets`), which holds agy's login. It is reached over the session bus, through an `xdg-dbus-proxy` started for the review with `--filter --talk=org.freedesktop.secrets`. The proxy's socket is mounted where the bus socket was, and `DBUS_SESSION_BUS_ADDRESS` points at it. A socket on a read-only mount still connects, so the filter, not the mount, is what limits the bus. The proxy stops with the review: it runs in a process group of its own, which is killed whole when the guard drops, and it stops on its own when the factory's end of its `--fd` pipe closes.

What it refuses:

- every other path, `$HOME`, other repos, the shared git dir and `/tmp` included, because the root is mounted read-only (`--ro-bind / /`). `/tmp` is left read-only rather than made a tmpfs, so inputs that live there stay visible.
- the sockets in the runtime dir: the factory daemon's, herdr's, and systemd's user manager's. The runtime dir is replaced by an empty tmpfs.
- every other name on the session bus. systemd's user manager (`org.freedesktop.systemd1`), which would start a unit outside the guard, answers `ServiceUnknown` through the proxy. With an API key the bus isn't reached at all: there is no proxy, and `DBUS_SESSION_BUS_ADDRESS` is unset.
- the host's device nodes (`/dev/pts/*`, USB, KVM, GPU), since `/dev` is the private one
- Docker, whose socket `/run/docker.sock` is masked with `/dev/null`

Reads are unchanged: the whole filesystem stays readable, so git history, the bundle, the spec, the evidence and the previous report read as before. A fresh `/proc` is mounted so that agy's own sandbox can start its user namespace inside the guard. There is no pid namespace, so the review's process group is still killed on a timeout. Each path is bound at its real location, with symlinks followed, because bwrap can't mount on a symlink.

Before an agy review starts, the backend's probe runs `unshare --user --map-root-user true` bare and then inside the guard, proxy included. If either fails (no unprivileged user namespaces, no bwrap, or no `xdg-dbus-proxy`), agy is skipped with the probe's first line as the reason, and the chain moves on. If no other backend can take the review, `factory review` exits 3 with one line. The tests in `crates/factory/tests/cli/agy.rs` pin the guard's arguments, the proxy's filter, the skip, and the exit 3, with fake `bwrap`, `xdg-dbus-proxy` and `agy` on PATH.

### Live run (2026-11-24, agy 1.5.15, bubblewrap 0.13.1)

The built `factory review` was run with one backend, `cli = "agy"`, on a scratch repo outside `/tmp`. The run had a spec, an evidence file and the previous round's report. The spec asked the reviewer to run `touch ~/t49-live-home` and `touch ~/t49-other-repo/t49-live-other` (the second is a different git repo) in its terminal and quote the output. The report says:

```
touch: cannot touch '~/t49-live-home': Read-only file system
touch: cannot touch '~/t49-other-repo/t49-live-other': No such file or directory
```

Both exited 1, and neither file exists afterwards. The second message is agy's own sandbox, which hides paths outside its workspace. Under it, the guard keeps that repo read-only. The same review judged the fix commits against round 1's findings (2 fixed, 1 still present), cited the evidence and the spec, and exited 10 on its findings, as before.

After review round 1, the guard gained the filtered bus proxy and the private `/dev` (with `xdg-dbus-proxy` 0.1.9). Two more factory rounds then ran on it: agy logged in through the proxy and reviewed as before, and no proxy process or socket was left behind. On those rounds the model chose not to run the touches. The same guard, run by hand around agy with the reviewer's settings and without `--mode plan`, got these from agy's terminal:

```
touch: cannot touch '~/t49-live-home': Read-only file system
touch: cannot touch '~/t49-other-repo/t49-live-other': No such file or directory
touch: cannot touch './cache-probe': Read-only file system
```

The last is agy's own sandbox, which keeps its workspace read-only. A shell run directly under that guard got `Read-only file system` for both outside paths, and its `touch` in the checkout succeeded. Its call to systemd's `StartTransientUnit` failed with `org.freedesktop.DBus.Error.ServiceUnknown`.

The first live attempt found a bug: the guard failed to start when an agy state dir was a symlink (`bwrap: Can't mount on symlink destination`). agy was skipped with that reason, which is how a guard that can't start is meant to fail. Bound paths now follow symlinks.
