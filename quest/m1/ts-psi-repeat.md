# [M] TS export never bunches repeated program tables

## Goal

`moq export ts` never lays copies of the program tables back to back where
they would overrun the PAT's or a PMT's 512-byte transport buffer, drained at
Rxsys = 1 Mb/s (ISO 13818-1 2.4.2.3). Every copy that marks its own tune-in
point still goes out, and changed tables still go out in order. Export carries
every video rendition and writes the tables ahead of each video keyframe, so an
ABR ladder of three or more renditions with aligned keyframes overruns the
PAT's buffer today. At 25 Mb/s, three PAT and PMT pairs back to back deliver
564 B of PAT in about 301 µs, of which about 38 B drains, peaking near 526 B.
Two renditions' 376 B fit.
`compliance.py` grades the program tables' buffers too, so every TS Interop arm
catches a regression.

## Plan

Decided (2026-10-10), from Codex's review of #5207 and kixelated's reply
there. `Slot::layout` (`rs/moq-mux/src/container/ts/schedule.rs`) lays the
tables just ahead of the first packet muxed after them, so when a heavier lane
goes first, every table group in the slot flushes at once. `main` bunched them
the same way at high fill before #5207.

- Start from the diff in
  [the review thread](https://github.com/moq-dev/moq/pull/5207#discussion_r4239051134):
  a run of tables that the same flush would lay right behind an identical run
  is dropped and padded back with nulls at the end of a padded slot, and
  `Slot::nulls` becomes `Option<usize>` so an unpadded slot gains none. A slot
  still opens with its clock packet and keeps its length, so no PCR moves.
  Carrying each table once per slot was tried and reverted in #5207: it dropped
  the copies that lead each send-ahead unit, breaking the tune-in points
  `aac_program_config_follows_each_table` checks.
- A new table version that bunches with the old one stays as rare as it is;
  spacing it out is not in scope.
- `compliance.py` gains a hard system-buffer check for the PAT and every PMT
  PID: TBsys of 512 B drained at Rxsys, then Bsys, per ISO 13818-1 2.4.2.3. Its
  strict T-STD check models only elementary streams today. DVB SI (SDT, EIT)
  and SCTE-35 stay out, since ISO defines those buffers for program-specific
  information. Every existing TS arm still passes.
- Test: a unit test runs laid slots' PAT and each PMT PID through its own
  512-byte buffer drained at 1 Mb/s, with keyframes aligned at 25 Mb/s on three
  video PIDs, which fails without the fix, and on two, which passes either way;
  plus the diff's `a_repeated_table_run_is_laid_once`.
