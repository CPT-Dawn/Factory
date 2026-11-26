# Flakes

Each test that failed once and passed again, and what made it.

## 2026-11-26: lab_ready::two_phones_of_one_model_are_told_apart_by_the_end_of_their_serials

- **Symptom**: CI on PR #76 (2d695b8) failed at `lab_ready.rs:327`: the first phone was said "Pixel 8 won't stay awake" instead of "Pixel 8 needs unlocking", so it never came to "ready".
- **Cause**: the test harness, not the daemon. `World::apply` changed a world file with `fs::write`, which empties the file before it writes the new text, and `plug` changed `devices` in two such writes. The test plugs the second phone while the daemon preps the first: the fake adb read `devices` while it was empty, said the first phone was not found, and its prep (which turns on stay-awake) failed. The daemon ignores a failed prep, so the probe that followed found stay-awake off.
- **Fix**: every world step is one change: the new text is written beside the file and renamed over it, under the same `<file>.lock` the fake adb takes for its settings writes, and `plug` and `setting` replace their line in one rewrite. `harness::a_world_step_never_shows_the_fakes_a_half_written_world` failed 17 times in 150 calls before the fix.
