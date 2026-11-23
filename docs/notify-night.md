# Event delivery into Claude Code sessions: the supervised night (#44)

`factory wait ci|lead --notify` returns at once, and the daemon posts one line into the
Claude Code session that asked when the wait ends. The delivery is off in the shipped config.
This page is what the owner checks before the first night with it on, and what was proven live.

## What was proven

A real Claude Code session (v2.1) registered `factory wait ci 12 --notify` from its own shell,
went idle, and the daemon posted the line when the checks finished. The session received it as
its own turn and answered it:

```
Another Claude session sent a message:
  factory wait #1 ended (exit 0): acme/shop#12: 1 passed, 0 failed
```

- `CLAUDE_PID` resolves to the session: the pid and start time factory stored match the
  session's own registry entry (`~/.claude/sessions/<pid>.json`), so a session that exits clears
  its wait at once.
- The frame the daemon writes (an optional auth line, then one `{"type":"user",...,"from":"factory"}`
  line) is accepted as it is. No format change was needed.
- Delivery is the receiving session's call, not factory's. See "The hold" below.

## The hold: the one thing that bites

Claude Code treats a message from another session as a peer message. The daemon is not a child
of the session and cannot assert a permission mode, so a session that **bypasses permission
prompts** (`--dangerously-skip-permissions`, or "bypass permissions on" in its footer) does not
deliver it. It holds it: the session shows "Held peer message ... not delivered to Claude" and an
approval dialog. Nobody at the keyboard means the dialog expires (about 5 minutes) and the line is
dropped. A session that prompts for permissions delivers it without any of this.

The fix is on the receiving session, in its settings (user, project, or `--settings`):

```json
{ "crossSessionInbound": "accept" }
```

With it, delivery was proven twice over: in a headless session, and in an interactive one where
approving the held line from the dialog released it into the queue and the session answered it.
Adding mode fields to the posted frame does not lift the hold; do not try.

## Checklist for the first supervised night

Do these with the owner at the machine, before leaving anything unattended.

1. **Switch it on.** In `~/.config/factory/config.toml` set `[notify] enabled = true`. Until then
   `--notify` is refused with exit 3 and one line naming this file. The shipped default stays
   `false`.
2. **Restart the daemon** so it reads the config: `systemctl --user restart factory.service`.
3. **Accept inbound messages** in every session that will register waits (leads and the
   implementers that use `--notify`): `"crossSessionInbound": "accept"` as above. Sessions that
   do not bypass permissions need nothing.
4. **Check the session sees its socket.** In that session's shell,
   `echo $CLAUDE_CODE_MESSAGING_SOCKET $CLAUDE_PID` prints a `cc-socks` path and a pid. If they
   are empty, the session is not one `--notify` can reach, and registering fails with exit 3.
5. **Register a short wait and watch it land.** Ask the idle session to run
   `factory wait ci <pr> --notify` on a PR whose checks are about to finish (or
   `factory wait lead <slug> --notify`). It prints `wait #N on ... registered`.
   `factory status` shows `waits: 1` and a row naming the wait.
6. **Confirm delivery.** The session shows `factory wait #N ended (exit 0): <repo>#<pr>: ...` as a
   message from another session and acts on it. If it shows "Held peer message" instead, step 3
   is missing for that session.
7. **Kill test.** Register a wait, exit the session, and check `factory status` shows
   `waits: none` at once.
8. **Restart test.** Register a wait, `systemctl --user restart factory.service`, and check
   `factory status` still shows it and the line still arrives.
9. **Leave a night running** only after 5 to 8 pass. In the morning, `factory status` should show
   `waits: none`, and every ended wait should have a line in its session.

## Rollback

Set `[notify] enabled = false` (or delete the `[notify]` table) and restart the daemon (`systemctl --user restart factory.service`). A wait
registered before is dropped unsaid when it ends; new `--notify` waits are refused, and the
blocking `factory wait ci|lead` works as before.

## What the session can and cannot be told

The posted line is text, not a command: Claude Code tells the receiving session to treat it as a
teammate's note within its own permissions. A wait that times out posts its timeout line the same
way (`exit 124`), so a session never waits on silence.
