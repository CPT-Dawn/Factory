# Flakes

Each test that failed once and passed again, and what made it.

## 2026-11-26: lab_ready::two_phones_of_one_model_are_told_apart_by_the_end_of_their_serials

- **Symptom**: CI on PR #76 (2d695b8) failed at `lab_ready.rs:327`: the first phone was said "Pixel 8 won't stay awake" instead of "Pixel 8 needs unlocking", so it never came to "ready".
- **Cause**: the test harness, not the daemon. `World::apply` changed a world file with `fs::write`, which empties the file before it writes the new text, and `plug` changed `devices` in two such writes. The test plugs the second phone while the daemon preps the first: the fake adb read `devices` while it was empty, said the first phone was not found, and its prep (which turns on stay-awake) failed. The daemon ignores a failed prep, so the probe that followed found stay-awake off.
- **Fix**: every world step is one change: the new text is written beside the file and renamed over it, under the same `<file>.lock` the fake adb takes for its settings writes, and `plug` and `setting` replace their line in one rewrite. `harness::a_world_step_never_shows_the_fakes_a_half_written_world` failed 17 times in 150 calls before the fix.

## 2026-11-26: bar::the_bar_runs_no_external_command_and_asks_herdr_nothing, status::a_status_runs_no_external_command_and_asks_herdr_nothing

- **Symptom**: CI on PR #76 (616aef9) failed at `bar.rs:444`: the tools' call counts read `[7, 13, 0, 0, 0, 0, 0, 1]` where the test had taken `[7, 11, 0, 0, 0, 0, 0, 1]`: two docker calls (one sweep) happened while only `factory bar` and the feed ran.
- **Cause**: the daemon, not the test. `Lab::notify` woke the stack sweep (which asks docker once per slot) as well as the waiting requests, and the phone watcher called it for every phone change: a list from adb, a probe, the end of a stream. The test plugs a phone, waits until the slot row and the bar show it, then counts calls. The phone's probe is written after the row shows, so its notify, and the docker sweep it woke, could land after the counts: a sweep the phone had no business causing, since no stack is one phone's.
- **Fix**: `notify` is split. `notify_waiters` bumps the change channel for waiting requests only and is what the phone watcher calls; `notify` is that plus the stack sweep, for what a stack can feel. A request being queued, which makes no slot, calls `notify_waiters` too, so the lease's grant is the one wake left for the sweep of an `acquire --no-device`. 720 runs under load (single-test loops pinned to one core beside the cli suite) had no failure; before the fix, runs under the same load failed now and then. `status::a_status_runs_no_external_command_and_asks_herdr_nothing` (a sibling test with no phone) failed the same way in r1fix's full runs: the first list adb's stream shows, and the lease just granted, each woke a sweep that could land after the counts. The notify split covers the first; the test now waits, as the bar test does, until the bar shows the slot checked, which ends the sweep the grant woke (1 failure in 192 runs before, none in 480 after both).

## 2026-11-26: lab::an_emulator_a_dead_daemon_left_booting_is_stopped_and_forgotten_never_ready

- **Symptom**: seen once in a full local run of the cli suite (t69), passing on the re-run. Pinned to one core beside parallel loops it failed 4 times in 224 runs, on the resumed acquire being told its emulator was ready while its boot was still held.
- **Cause**: two things, both in the world around the daemon. The fakes: `adb emu kill`, `systemctl stop` and `systemd-run` changed `devices`, `units` and `unit-ports` by plain read-modify-write, so a boot racing a stop lost a line; and a kill ended the unit before adb forgot the device, though real adb drops the device first. The test: it un-stalled the new daemon's boot as soon as the first systemd-run call showed, but the daemon's view of adb (the fake's `track-devices` polls every 50 ms) still held the killed emulator's serial, which the new boot reuses, so the boot's look found the old emulator booted and said ready.
- **Fix**: the golden fakes change those files under `<file>.lock` (`unit-ports` under the units lock, with its unit), `yank` included, and a kill or stop forgets the device first and ends the unit last (internal atomicity and order only; no golden transcript changes). The test waits for the second systemd-run call and for `World::settle_trackers` before it un-stalls the boot. `harness::a_killed_emulator_is_gone_from_adb_before_its_unit_is` failed before the fakes fix. 224 of 224 runs passed on one core after, where 4 had failed before.
- **Open**: a boot's look may still use a stale frame of the adb stream that shows a killed emulator's serial, if the serial is reused inside the stream's poll window. Real adb pushes a removal at once, so this is not expected outside the fake.
- **Open**: once in 20 loaded full runs (35 s instead of 19 s) the test failed waiting for the second systemd-run call. Not reproduced in 300 more runs beside heavy load; the likely cause is the caller's 20 s resume window (`RESUME_TIMEOUT`) running out while the loaded machine drains the old daemon (10 s) and starts the new one, so no acquire is left to boot the emulator.

## 2026-11-26: lab_ready::two_phones_of_one_model_are_told_apart_by_the_end_of_their_serials (second flake)

- **Symptom**: `lab_ready.rs:350`: the two last notices were `["Pixel 8 (NE01) locked again", "Pixel 8 (NE02) unplugged"]`, in the other order; 58 failures in 192 runs pinned to one core, 5 in 24 full runs beside each other.
- **Cause**: the test, not the daemon. It unplugged one phone and locked the other in two steps with nothing between them. The unplug reaches the daemon through adb's stream (polled every 50 ms by the fake) and the lock through the phone's next probe, so the two are told in whichever order the daemon sees them.
- **Fix**: the test waits for the unplug's notice before it locks the other phone. 0 failures in 192 runs after. This failed the same way before the notify split of the bar flake: it is not a product change.

## 2026-11-26: open, not fixed

- `cutover::the_cutover_says_what_it_did_in_json`: once in 24 full runs beside each other, the cutover said "today's devlab did not answer ... cannot start it: Text file busy (os error 26)". Suspected cause: the test writes its fake devlab script and another thread's fork inherits the still-open write end before the exec (ETXTBSY), so the exec fails; closing the writer before the exec, or a retry, would settle it.
- `claude::the_statusline_command_is_gone_from_the_cli_and_its_help`: seen once with a BrokenPipe while writing to the command's stdin. Suspected cause: the command exits (unknown subcommand) before the test has written its input. Not looked into.
- `lab::an_emulator_a_dead_daemon_left_booting_is_stopped_and_forgotten_never_ready`: the load-only failure noted in its entry above.
