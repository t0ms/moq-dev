#!/usr/bin/env bash
# MPEG-TS / IRD compliance harness for the `ts-pacer` crate.
#
# Where `test/ts` round-trips a stream through a relay and analyses the
# subscriber's (VBR, un-paced) `export ts` output, this harness validates the
# downstream grooming stage in isolation: it takes a transport stream, runs it
# through ts-pacer's offline CBR shaper (`cargo run -p ts-pacer --example
# cbr_file`), and checks that the paced output is something a hardware IRD would
# accept. It reuses the same analyzer as `test/ts` (`../ts/compliance.py`) plus a
# TSDuck `pcrverify` PCR-accuracy gate, which is the check a byte-locked CBR
# groomer exists to pass and a bursty re-mux fails.
#
# Modes:
#   ./run.sh                      # generate a clip, pace it, analyze
#   ./run.sh --source cap.ts      # pace a real capture instead
#   ./run.sh --bitrate 12000000   # force the target mux rate (default: source * 1.2)
#   ./run.sh --pcr preserve       # preserve source PCR instead of byte-locking
#   ./run.sh --strict             # also fail on broadcast-shape warnings
set -euo pipefail

DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
WORKSPACE=$(cd "$DIR/../.." && pwd)
COMPLIANCE="$DIR/../ts/compliance.py"

SOURCE=""
DURATION="${TSP_DURATION:-15}"
BITRATE="" # target mux rate; derived from the source when unset
PCR_MODE="regenerate"
PCR_JITTER_US="${TSP_PCR_JITTER_US:-500}"
PROFILE="${TSP_PROFILE:-release}"
STRICT=""
PASSTHRU=()

FFMPEG="${FFMPEG_BIN:-ffmpeg}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --source) SOURCE="$2"; shift 2 ;;
        --duration) DURATION="$2"; shift 2 ;;
        --bitrate) BITRATE="$2"; shift 2 ;;
        --pcr) PCR_MODE="$2"; shift 2 ;;
        --pcr-jitter-us) PCR_JITTER_US="$2"; shift 2 ;;
        --strict) STRICT="--strict"; shift ;;
        *) PASSTHRU+=("$1"); shift ;;
    esac
done

have() { command -v "$1" >/dev/null 2>&1; }

require_tools() {
    local missing=() t
    for t in tsp tsanalyze python3 cargo; do
        have "$t" || missing+=("$t")
    done
    [[ -n "$SOURCE" ]] || have "$FFMPEG" || missing+=("$FFMPEG")
    if [[ ${#missing[@]} -gt 0 ]]; then
        echo "error: missing required tools: ${missing[*]}" >&2
        echo "  TSDuck (tsp, tsanalyze) is required; install from https://tsduck.io" >&2
        exit 1
    fi
}

require_tools

TMP=$(mktemp -d)
# shellcheck disable=SC2329  # invoked via trap
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

SRC_TS="$TMP/source.ts"
PACED_TS="$TMP/paced.ts"

if [[ -n "$SOURCE" ]]; then
    [[ -f "$SOURCE" ]] || { echo "error: no such source: $SOURCE" >&2; exit 1; }
    echo "### using source $SOURCE"
    cp "$SOURCE" "$SRC_TS"
else
    # A VBR clip (no --muxrate, so no source stuffing) resembles a MoQ subscriber's
    # `export ts` output: real media packets with no null padding, which the pacer
    # then shapes to CBR.
    echo "### generating ~${DURATION}s VBR clip with ffmpeg"
    "$FFMPEG" -y -hide_banner -loglevel error \
        -f lavfi -i "testsrc=size=1280x720:rate=25" \
        -f lavfi -i "sine=frequency=1000:sample_rate=48000" \
        -t "$DURATION" \
        -c:v libx264 -profile:v high -preset veryfast -pix_fmt yuv420p \
        -x264-params "keyint=25:min-keyint=25:scenecut=0" -b:v 4M \
        -c:a aac -b:a 128k \
        -f mpegts -pes_payload_size 0 "$SRC_TS"
fi

# Default the target mux rate to 1.2x the source bitrate so content never
# out-runs the byte clock (which would overflow the jitter buffer and drop).
if [[ -z "$BITRATE" ]]; then
    SRC_BR=$(tsanalyze --json "$SRC_TS" 2>/dev/null |
        python3 -c 'import json,sys; d=json.load(sys.stdin); print(int(d["ts"].get("bitrate") or d["ts"].get("pcr-bitrate") or 0))')
    if [[ -z "$SRC_BR" || "$SRC_BR" -le 0 ]]; then
        echo "error: could not derive source bitrate; pass --bitrate" >&2
        exit 1
    fi
    BITRATE=$(( SRC_BR * 12 / 10 ))
fi
echo "### target mux rate ${BITRATE} b/s, PCR mode ${PCR_MODE}"

echo "### building ts-pacer (${PROFILE})"
flag=()
[[ "$PROFILE" == "release" ]] && flag=(--release)
(cd "$WORKSPACE" && cargo build ${flag[@]+"${flag[@]}"} -p ts-pacer --example cbr_file)
TARGET_BASE=$(cargo metadata --format-version 1 --manifest-path "$WORKSPACE/Cargo.toml" --no-deps |
    sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')
[[ -n "$TARGET_BASE" ]] || { echo "error: could not resolve cargo target directory" >&2; exit 1; }
BIN="$TARGET_BASE/$PROFILE/examples/cbr_file"

echo "### pacing $SRC_TS -> $PACED_TS"
"$BIN" "$SRC_TS" "$PACED_TS" "$BITRATE" "$PCR_MODE"
echo

# PCR-accuracy gate: a byte-locked CBR groomer must keep every PCR within the
# tolerance a hardware IRD's PLL allows. A bursty re-mux fails this even when the
# structural checks pass, so it is the harness's headline assertion. Only the
# regenerate mode promises byte-accurate PCR; preserve mode intentionally keeps
# the source values (for a soft IRD that re-buffers), so the gate is informational
# there rather than a hard failure.
echo "### pcrverify (jitter-max ${PCR_JITTER_US} us)"
PCRV=$(tsp -I file "$PACED_TS" -P pcrverify --jitter-max "$PCR_JITTER_US" -O drop 2>&1 | tail -1)
echo "  $PCRV"
OVER=$(echo "$PCRV" | sed -n 's/.*OK, \([0-9,]*\) with jitter.*/\1/p' | tr -d ',')
if [[ "$PCR_MODE" == "regenerate" && -n "$OVER" && "$OVER" -gt 0 ]]; then
    echo "error: $OVER PCR(s) exceeded the ${PCR_JITTER_US} us accuracy tolerance" >&2
    exit 1
elif [[ -n "$OVER" && "$OVER" -gt 0 ]]; then
    echo "  note: ${PCR_MODE} mode does not byte-lock PCR; ${OVER} over tolerance (informational)"
fi
echo

# Full structural + shape report, with the source as the duration-fidelity
# reference so a mis-scaled clock is caught (a self-consistent PCR on the wrong
# rate passes every other timing check).
echo "### compliance report"
python3 "$COMPLIANCE" --ts "$PACED_TS" --reference "$SRC_TS" $STRICT ${PASSTHRU[@]+"${PASSTHRU[@]}"}
