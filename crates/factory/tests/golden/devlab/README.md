# Golden transcripts of devlab

Each `*.golden` file is a scenario played against devlab in a sandbox, and what each devlab call
showed: stdout, stderr and the exit code. They were recorded from today's TypeScript devlab; the
Rust `devlab` must reproduce them byte for byte. A change to a transcript is a change in
behaviour, and its commit says so.

## Replaying

```sh
# against today's devlab
GOLDEN_DEVLAB=$HOME/.local/bin/devlab cargo test --test cli transcript
# against any other binary; it is linked into the sandbox as `devlab`
GOLDEN_DEVLAB=target/debug/factory cargo test --test cli transcript::slots_
```

Without `GOLDEN_DEVLAB` the transcript tests pass with a note that they were skipped. A failure
names the transcript line, the step, the stream and the first byte that differs, and shows both
outputs. `GOLDEN_RECORD=1` rewrites the transcripts with what the binary showed instead.

Groups: `slots-*` (stack slots, #14), `devices-*` (phones and emulators, #16), `cli-*` (usage
and help). Every transcript has a test in `tests/cli/devlab_golden.rs`.

## The sandbox

A call runs with a fresh HOME, XDG state, config and runtime dirs inside a temp dir, and a PATH
whose first entry holds fakes of adb, docker, systemctl, systemd-run, avdmanager, supabase, git and
notify-send (`fakes/*.sh`). They play back the state in the sandbox's `world` directory, which
the transcript's world steps change. Nothing reaches a real device, container, unit or devlab
database. The machine config is `machine.json` (one emulator, no memory floor); a worktree whose
name starts with `demo-` holds `project/` (devlab.json and a supabase config), any other is
empty.

## Format

One step per line:

- `# ...` and blank lines are notes.
- `$ <owner>@<worktree> <shell>` runs `<shell>` with `sh -c` in worktree `<worktree>`, for
  owner `<owner>` (a live process named `claude`, given to devlab in `DEVLAB_OWNER_PID`).
- `& <tag> <owner>@<worktree> <shell>` starts such a call in the background; `until <tag> <text>`
  waits until its output shows `<text>`; `wait <tag>` waits for it to end.
- `kill <owner>` ends the owner's process.
- World steps: `plug <serial> <state> [<prop>...]` (adb lists it; plugging again replaces it),
  `unplug <serial>`, `lock <serial>` (its lock screen stays up), `break <op>` and `mend <op>`
  where `<op>` is `supabase start` or `systemd-run`.

After a `$` or `wait` step come the lines it showed: `1> ` per stdout line, `2> ` per stderr
line, `1~ ` or `2~ ` for a last line without a newline, and `? <exit code>`.

What the sandbox cannot fix is normalised: its path shows as `<sandbox>`, owner `a`'s pid as
`claude:<a>`, and free memory as `<mem>`.

Today's devlab reuses the highest lease id once that lease ends; no transcript asks for a lease
after its highest one was released, so a devlab with ids that only grow replays them too. Slot
ports come from `project/` in the 27000s; a port in use on the machine would move a slot.
