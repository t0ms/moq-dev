# ts-pacer compliance harness

Validates the [`ts-pacer`](../../rs/ts-pacer) crate's constant-bitrate grooming
in isolation: it shapes a transport stream through the crate's offline CBR
example (`cargo run -p ts-pacer --example cbr_file`) and checks that the paced
output is something a professional IRD would accept.

Where [`test/ts`](../ts) round-trips a stream through a relay and analyses the
subscriber's un-paced `export ts` output, this harness tests the **downstream
pacing stage only** (no relay, no network), so it is fast and deterministic.

## What it checks

1. **`pcrverify` (headline gate).** A TSDuck `tsp -P pcrverify` pass at a tight
   PCR-accuracy tolerance (default 500 us). Byte-locked CBR output passes; a
   bursty re-mux fails. Any PCR outside tolerance fails the harness.
2. **Full compliance report.** Reuses [`../ts/compliance.py`](../ts/compliance.py)
   with the source as the duration-fidelity reference: PAT/PMT, PSI CRC,
   continuity, PCR presence/monotonicity, PCR repetition/jitter, null ratio,
   T-STD, and that the paced clip's duration matches the source (a self-consistent
   PCR on the wrong rate is caught here).

`[hard]` checks fail the run; `[shape]` checks warn unless `--strict`.

## Running

```bash
just test ts-pacer                     # generate a clip, pace it, analyze
just test ts-pacer --source cap.ts     # pace a real capture instead
just test ts-pacer --bitrate 12000000  # force the target mux rate
just test ts-pacer --pcr preserve      # preserve source PCR (no byte-lock/re-insert)
just test ts-pacer --strict            # also fail on broadcast-shape warnings
```

Requires TSDuck (`tsp`, `tsanalyze`), `python3`, `cargo`, and (when generating a
clip) `ffmpeg`. The default target mux rate is 1.2x the source bitrate so content
never out-runs the byte clock.

## Notes on PCR

- **Regenerate mode** (default) byte-locks every PCR to its output position and
  re-inserts extra PCR-only packets on the PCR PID when the source's PCR is
  sparser than the 40 ms repetition limit. This is what a hardware IRD's PLL and
  PCR-accuracy checks require.
- **Preserve mode** keeps source PCR values verbatim and only paces
  transmission, so it inherits the source's PCR cadence (fine for soft IRDs that
  re-buffer, not for a CBR/ASI hardware IRD).
