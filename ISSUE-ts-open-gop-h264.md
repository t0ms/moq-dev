# moq can't round-trip open-GOP broadcast H.264 (recovery-point keyframes)

## Summary

`moq import ts` / `moq export ts` cannot round-trip a real broadcast MPEG-TS
feed (a CNN International EMEA HD capture) whose H.264 uses **recovery-point SEI
open-GOP random access instead of IDR frames**. The H.264 splitter only treats an
`IdrSlice` (NAL type 5) as a keyframe, so on this stream it almost never flags
one. As a result the importer never publishes a video *rendition*, and
`export ts` then aborts on its SCTE-35 program-clock guard:

```
Error: TS export of section-framed verbatim streams (e.g. SCTE-35) requires a video track for the program clock
```

This surfaced while validating a real capture through the merged `test/ts`
compliance harness (`just test ts --source ~/CNNiEMEA2.ts`). The harness did its
job: the source file is clean, but the moq round-trip fails.

## The stream

`~/CNNiEMEA2.ts` (service "CNNI EMEA HD"), PMT PID 100, PCR PID 111:

| PID | Type | Notes |
|---|---|---|
| 111 | 0x1B AVC video | 1920x1080 high@4.0, also the PCR PID |
| 121 | 0x03 MPEG-1 audio (MP2) | decoded |
| 123 | 0x06 private (AC-3 descriptor) | carried verbatim (PES) |
| 131 | 0x06 private (Teletext descriptor) | carried verbatim (PES) |
| 141/142/143 | 0x86 SCTE-35 Splice Info | section-framed verbatim (program-level CUEI) |

NAL histogram over a ~15s / 181,524-video-packet slice:

```
1193 Aud   1193 Sei   1192 NonIdrSlice   54 Pps   26 Sps   1 IdrSlice
```

26 SPS and 54 PPS but only **1 IDR**. The random-access points are non-IDR
I-slices flagged by recovery-point SEI, which is completely standard for
broadcast contribution/distribution H.264.

## Root cause

The Annex-B splitter marks a frame as a keyframe purely from the IDR NAL type:

- `rs/moq-mux/src/codec/h264/split.rs` `decode_nal`: only `Avc3NalType::IdrSlice`
  sets `self.current.contains_idr = true`.
- `maybe_start_frame` then sets `keyframe = self.current.contains_idr`.

The importer only resolves the codec config (and therefore publishes the catalog
video rendition) from an SPS carried on a `keyframe` frame:

- `rs/moq-mux/src/codec/h264/import.rs` `write_frames`:
  `if !self.avc1 && frame.keyframe && let Some(sps) = find_sps(...) { configure_from_sps(...) }`.
- A non-keyframe before the first config is tolerated but produces no rendition;
  a keyframe with no config is a hard error.

So for an open-GOP stream the splitter emits a long run of non-keyframes, the
importer never configures, and `catalog.video.renditions` stays empty.

The failure then gets *reported* by the exporter, which is a red herring:

- `rs/moq-mux/src/container/ts/export.rs` `build_psi` computes `needs_clock`
  (any section-framed verbatim stream, i.e. SCTE-35) and requires a video track
  when so. The section-framed SCTE-35 tracks register in the catalog immediately
  on PMT parse (`register_verbatim`), before any video rendition exists, so the
  exporter builds its PSI, sees SCTE-35 + no video, and aborts.

Without SCTE-35 the export wouldn't error, but there'd still be no video: it
would emit an audio-only program. `export fmp4` confirms this independently,
failing with `cmaf: can't synthesize CMAF init for audio codec Mp2` because the
only rendition that ever appears is the MP2 audio track.

## Reproduction

```bash
just test ts --source ~/CNNiEMEA2.ts     # fails at export (needs a real capture)

# The source itself is well-formed (all hard checks pass):
tsp -I file ~/CNNiEMEA2.ts -P until --packets 200000 -O file /tmp/cnn.ts
./test/ts/run.sh --analyze-only /tmp/cnn.ts   # ts: PASS

# NAL histogram showing open-GOP (1 IDR, many SPS/PPS):
#   RUST_LOG=moq_mux::codec::h264=trace on `cat /tmp/cnn.ts | moq ... import ts`
```

Track set the importer produced (relay round-trip): `catalog.json` + verbatim
`0.ts..4.ts` (AC-3, teletext, 3x SCTE-35). No `.avc3` video, no MP2 rendition
survived to the exporter before it aborted.

## Impact

- Real broadcast H.264 (open-GOP / recovery-point) can't be ingested to a usable
  MoQ broadcast: no video rendition, so nothing downstream (TS/fMP4/HLS export,
  players) can render it. This is not a niche encoder quirk; it's the norm for
  contribution feeds.
- The user-facing error blames SCTE-35, which is misleading. The real problem is
  keyframe detection.

## Possible directions (not implemented)

Ordered roughly by scope. All touch the H.264 codec path, so they target `dev`
(breaking/behavioral change to `rs/moq-mux`) and want new unit tests in
`split.rs` / `import.rs` plus a harness case (a synthetic open-GOP clip).

1. **Recognize recovery-point random access as a keyframe.** Parse the
   recovery-point SEI (payload type 6) and treat an I-slice access unit carrying
   one (or an all-intra AU that re-presents SPS/PPS) as a keyframe in the
   splitter. This is the correct fix but needs careful SEI parsing and a clear
   definition of "keyframe" for open-GOP (recovery vs exact).
2. **Let the importer configure from any SPS, not just a keyframe SPS**, so the
   rendition appears even before the first true keyframe. Decouples "catalog
   rendition exists" from "first group starts"; smaller, but doesn't fix group
   boundaries / tune-in alignment on its own.
3. **Harden the exporter's readiness** so it waits for announced media renditions
   before concluding a program has no video (the SCTE-35 guard races the
   incremental catalog). This removes the misleading error but not the underlying
   no-video problem.

(1) is the substantive fix; (2)/(3) are complementary robustness improvements.

## Relationship to #1979

Closely related, but a **distinct failure mode with a different fix locus**, so
this is a separate issue (cross-link #1979, don't fold in).

- #1979 ("catalog convergence race locks PSI before video track resolves") is an
  **exporter-side timing race**: the video rendition *does* resolve, just late
  (on the first keyframe's inline SPS), and if an audio-only snapshot reaches the
  exporter first, PSI locks audio-only and the late video trips
  `TS track layout changed after PAT/PMT was emitted`. Its proposed fixes wait
  for a "stable"/PMT-complete catalog before locking PSI.
- This issue is **importer-side**: for open-GOP H.264 the video rendition
  *never* resolves (no IDR -> the splitter never flags a keyframe -> the importer
  never configures), so it's not a race. Given unlimited time the video still
  never appears.
- Shared root: both trace to H.264 gating catalog-rendition resolution on the
  first keyframe's inline SPS (`rs/moq-mux/src/codec/h264/import.rs`), which
  #1979 cites directly.
- **Caveat to raise on #1979**: t0ms's "wait for the PMT-declared track count"
  proposal would make the exporter **hang forever** on an open-GOP program (the
  PMT declares the video PID, but its rendition never resolves). So #1979's
  exporter-readiness fix needs this importer fix (or a timeout/fallback) to be
  safe on real broadcast feeds.

## Regression?

No. The IDR-only keyframe logic (`contains_idr = true` set only for
`Avc3NalType::IdrSlice`) has been present since the moq-mux backport landed the
H.264 splitter (#1918); it never handled recovery-point open-GOP. This is a
latent gap in a newish feature (TS ingest of real broadcast H.264), not a
regression from previously working behavior. It "looks new" only because verbatim
TS carriage (#1842) and the SRT gateway (#1915) are recent, so real broadcast
feeds with SCTE-35 + open-GOP have only recently been pushed through the path.

## Related

- Cross-package sync: any `rs/moq-mux` container/codec change should be mirrored
  per the table (js/hang, doc/concept) if it changes catalog/container behavior.
- Test coverage: `test/ts` now has a `--via-srt` mode; once fixed, add an
  open-GOP source (real capture or synthetic) as a regression fixture.
