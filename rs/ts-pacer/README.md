# ts-pacer

Transport-agnostic MPEG-TS constant-bitrate pacer for broadcast (IRD) egress.

Modern IP transports (MoQ, SRT, RIST, RTP, file playback) deliver MPEG-TS in
bursts. Professional Integrated Receiver/Decoders (IRDs) expect a smooth,
constant-bitrate transport with byte-accurate PCR. This crate is the missing
adaptation layer between the two: feed it already-multiplexed transport packets
and it emits a deterministic CBR stream a hardware IRD will accept.

It is **not** a muxer. It never demultiplexes, remultiplexes, rewrites PSI,
regenerates PAT/PMT, or touches continuity counters. PID structure and PES/PSI
payloads pass through untouched. It shapes *transmission timing* and, optionally,
byte-locks the PCR. The only bytes it ever rewrites are the six PCR octets (under
`PcrMode::Regenerate`).

Deliberately free of any `moq-*` / QUIC dependency: a MoQ subscriber is just one
possible source. Point it at SRT, RIST, a file, or a socket and nothing changes.

## How it works

Two clocks run side by side:

- a **transport byte clock** at the target mux rate. Output packet `n` leaves at
  `anchor + n * 188 * 8 / bitrate`, so the wire rate is exactly constant. This
  clock also byte-locks the regenerated PCR.
- a **media clock** recovered from the source PCR. Content packets are released
  at the source's own media rate, delayed by the configured latency, so the
  output preserves the source duration instead of draining a burst faster than
  real time.

Every output slot emits a content packet when the media clock says one is due and
the buffer has it, otherwise a null packet. So a burst is absorbed by a bounded
jitter buffer and released at media rate, while the wire stays CBR.

## Quick start

The engine is a `Source` -> pacer -> `Sink` pipeline. Any producer is one
`Source`, any output is one `Sink`.

```rust
use std::net::SocketAddr;
use ts_pacer::{Config, ReadSource, UdpSink, pace};

// A MoQ subscriber writing `export ts` to a pipe is just an AsyncRead source.
let source = ReadSource::new(tokio::io::stdin());
let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
let sink = UdpSink::new(socket, "239.0.0.1:5000".parse::<SocketAddr>()?);

let stats = pace(Config::new(10_000_000), source, sink).await?;
eprintln!("null ratio {:.1}%", stats.null_ratio() * 100.0);
```

Prefer to push packets yourself? `TsPacer` wraps the same engine behind a
background task:

```rust
use ts_pacer::{Config, CallbackSink, TsPacer};

let pacer = TsPacer::spawn(Config::new(10_000_000), CallbackSink::new(|dg: &[u8]| {
    // forward the datagram somewhere
    Ok(())
}));
pacer.push_bytes(&some_ts_bytes).await?;   // whole or partial 188-byte packets
let stats = pacer.close().await?;
```

## Configuration

Build a `Config` with `Config::new(bitrate)` (explicit rate) or `Config::auto()`
(derive the rate from the source), then chain the `with_*` setters. The struct is
`#[non_exhaustive]`, so use the constructors, not a struct literal.

| Setter | Type | Default | Meaning |
|---|---|---|---|
| `Config::new(bitrate)` | `u64` bits/s | required | Constant output rate over full 188-byte packets (content + stuffing). Must be at least the peak instantaneous input rate. |
| `Config::auto()` | -- | -- | Derive the rate from the source's measured content rate (see [Auto bitrate](#auto-bitrate)). |
| `.with_bitrate(Bitrate)` | `Bitrate` | -- | Set the rate target directly (`Bitrate::Constant(bps)` or `Bitrate::Auto { headroom }`). |
| `.with_latency(d)` | `Duration` | 200 ms | De-jitter priming: how long to fill the buffer before the first emit, and the steady-state cushion. Larger absorbs more burst at the cost of latency. |
| `.with_max_latency(d)` | `Duration` | 2000 ms | Hard cap on buffered media. Input past this depth is dropped oldest-first to keep latency and memory bounded. |
| `.with_pcr_mode(m)` | `PcrMode` | `Regenerate` | How the PCR is handled (see [PCR modes](#pcr-modes)). |
| `.with_packets_per_datagram(n)` | `usize` | 7 | Packets coalesced per output datagram. 7 * 188 = 1316 bytes fits a 1500-byte MTU over UDP/RTP. |
| `.with_pcr_max_interval(d)` | `Duration` | 40 ms | Max PCR repetition to hold on the output (TR 101 290 P1 limit). Under `Regenerate`, extra byte-locked PCR-only packets are inserted into stuffing slots when the source PCR is sparser than this. Ignored under `Preserve`. |

### PCR modes

- **`PcrMode::Regenerate`** (default) rewrites each PCR-bearing packet's PCR to
  the value implied by its output byte offset at the target rate, so
  `PCR == byte_offset * 8 * 27_000_000 / bitrate` by construction. This is what a
  CBR/ASI hardware IRD's PCR-accuracy and repetition checks require. Only the six
  PCR octets (and the discontinuity indicator across a genuine source
  discontinuity) are touched.
- **`PcrMode::Preserve`** leaves every PCR value byte-for-byte untouched and only
  paces transmission. A soft IRD / player that recovers the clock from PCR
  *values* and re-buffers plays this cleanly. Note that once nulls are stuffed to
  hit the rate, a preserved PCR no longer sits at the byte position a constant-rate
  demuxer expects, so a hardware IRD that checks PCR-vs-byte (`tsp -P pcrverify`)
  will flag it. Use `Preserve` only for soft players.

### Auto bitrate

You asked whether the output rate can just match the source instead of being
stated explicitly. Yes, with one caveat worth understanding:

**By the time TS comes out of a MoQ subscriber it's a re-mux.** MoQ carries only
media objects, so the source's original null padding is gone. If the original was
padded CBR (say 6 Mb/s = 1 Mb/s content + 5 Mb/s null), that 6 Mb/s *mux rate* is
not recoverable from MoQ, because the padding no longer exists. What **is**
recoverable, exactly, is the true **content bitrate**, measured from the PCR
clock, and that's almost always what you actually want for egress (reproducing
the dead padding just wastes bandwidth).

`Config::auto()` measures the incoming content rate over a short warm-up window (a
few PCR samples) and locks the output to that rate plus a headroom margin (default
15%, for VBR peaks and the pacer's own stuffing). The output is still true CBR;
only the *choice* of rate is automatic.

```rust
// Self-tuning: no explicit rate. Output CBR ~= source content rate + 15%.
let stats = pace(Config::auto(), source, sink).await?;

// Tune the headroom (e.g. 25% for peaky VBR):
use ts_pacer::Bitrate;
let config = Config::auto().with_bitrate(Bitrate::Auto { headroom: 0.25 });
```

Trade-offs: auto adds a little startup latency (the measurement window), and too
little headroom risks buffer overflow on VBR peaks while too much wastes
bandwidth on nulls. When you know the rate and want minimal latency, pass it
explicitly. When the source has no usable PCR to measure, auto falls back to
`DEFAULT_AUTO_FALLBACK` (4 Mb/s). `estimate_content_bitrate(&[Packet])` is exposed
if you want to measure a run of packets yourself.

## Sources and sinks

Built-in `Source`s:

- `ReadSource<R>` -- any `tokio::io::AsyncRead` (a pipe, socket, file, or process
  stdin such as `moq ... export ts`). Resynchronises on the `0x47` sync byte.
- `IterSource<I>` -- any `Iterator<Item = Packet>`, for in-memory / test vectors.

Built-in `Sink`s:

- `WriteSink<W>` -- any `tokio::io::AsyncWrite` (a pipe, file, or `stdout`).
  Writes the raw transport bytes so you can pipe the paced stream onward exactly
  like the subscriber itself (`... | pacer | ffplay -i -`).
- `UdpSink` -- raw MPEG-TS over UDP to a unicast or multicast destination.
- `RtpSink` -- RTP-encapsulated MPEG-TS (RFC 2250, payload type 33).
- `CallbackSink` -- hand each datagram to a closure for embedding.

Implement `Source` / `Sink` for anything else (a MoQ subscriber handle, an SRT
receiver, an ST 2022-7 pair, an FEC path) without touching the engine.

## Stats

`pace` returns, and `TsPacer::close` yields, a `Stats` snapshot:

`output_packets`, `content_packets`, `null_packets`, `dropped_packets` (buffer at
`max_latency`), `input_nulls_stripped` (source padding replaced by our own),
`underruns` (buffer starved), `pcr_rebases` (source discontinuities),
`pcr_inserted` (byte-locked PCR-only packets added to hold the repetition limit),
and `null_ratio()`.

## Examples

### Live MoQ subscriber -> paced stdout / UDP / RTP

The `moq_egress` example is a thin MoQ egress adapter: stdin -> pacer -> stdout,
UDP, or RTP. The MoQ subscriber is just a producer, so the whole thing is a pipe.
Take your working subscriber command and splice the pacer in before the player.

The simplest form pipes the paced stream straight on, just like the subscriber:

```bash
# Before: subscriber straight to a soft player.
./moq --client-tls-disable-verify \
      --client-connect https://34.246.187.61:443/anon \
      --broadcast cnn.international.emea.loop.hang \
      export ts --latency-max 5s \
  | ffplay -probesize 10M -analyzeduration 5M -vf bwdif -sync video -framedrop -i -

# After: subscriber -> ts-pacer -> ffplay, auto-rate, over a stdout pipe.
./moq --client-tls-disable-verify \
      --client-connect https://34.246.187.61:443/anon \
      --broadcast cnn.international.emea.loop.hang \
      export ts --latency-max 5s \
  | cargo run --release -p ts-pacer --example moq_egress -- - auto \
  | ffplay -probesize 10M -analyzeduration 5M -vf bwdif -sync video -framedrop -i -
```

Or push it to a multicast group for a hardware IRD:

```bash
./moq ... export ts --latency-max 5s \
  | cargo run --release -p ts-pacer --example moq_egress -- 239.0.0.1:5000 auto

ffplay -i 'udp://@239.0.0.1:5000'   # or point your IRD at the group
```

`moq_egress` arguments:

```text
moq_egress <-|stdout|dest_ip:port> <bitrate_bps|auto> [--rtp] [--preserve] [--latency-ms N]
```

- `<-|stdout|dest_ip:port>` -- `-` or `stdout` to write raw TS to a pipe, or a
  UDP/RTP destination (unicast or multicast group).
- `<bitrate_bps|auto>` -- explicit rate (e.g. `10000000`) or `auto` to derive it.
- `--rtp` -- RTP encapsulation instead of raw UDP (ignored for stdout).
- `--preserve` -- keep source PCR values (`PcrMode::Preserve`) instead of
  regenerating them. Use for soft players, not hardware IRDs.
- `--latency-ms N` -- de-jitter priming latency (default 200).

### ffplay and "RTP: dropping old packet received too late"

That warning is a receiver-side quirk, not stream corruption. ffmpeg's RTP
reorder buffer compares each packet's 16-bit sequence number to the last one it
emitted (`diff = seq - s->seq`) and drops anything that looks older; on a
restart, or under reordering/loss, many builds get stuck dropping every packet.
Raw MPEG-TS over UDP and a stdout pipe carry no RTP sequence numbers, so they
can't hit it: prefer `-` (stdout) or plain UDP (no `--rtp`) for soft players like
ffplay/VLC. If you do need RTP into ffplay, disable its reorder buffer:

```bash
ffplay -reorder_queue_size 0 -fflags nobuffer -i 'rtp://239.0.0.1:5000'
```

RTP is still the right choice for a hardware IRD that expects it; the reorder
buffer is specific to ffmpeg's software receiver.

### Offline CBR shaping (file in, CBR file out)

The `cbr_file` example paces a `.ts` file deterministically (no sockets, no wall
clock), which is what the compliance harness runs:

```bash
# Explicit rate:
cargo run -p ts-pacer --example cbr_file -- in.ts out.ts 12000000 regenerate
# Or derive it from the file:
cargo run -p ts-pacer --example cbr_file -- in.ts out.ts auto

# Verify byte-locked PCR accuracy:
tsp -I file out.ts -P pcrverify --jitter-max 500 -O drop
```

```text
cbr_file <in.ts> <out.ts> <bitrate_bps|auto> [preserve|regenerate]
```

## Compliance

`test/ts-pacer/` runs a TSDuck-based compliance harness (PCR accuracy, bitrate
stability, continuity, PAT/PMT integrity, TR 101 290). Run it via:

```bash
just ts-pacer            # from test/
```

See [`test/ts-pacer/README.md`](../../test/ts-pacer/README.md) for details.

## Roadmap

The `Source` / `Sink` split keeps the door open for ST 2022-7 seamless
redundancy, FEC, SRT/RIST output adapters, SCTE-35 splice monitoring, and NOC
telemetry, none of which the core pacer needs to know about.
