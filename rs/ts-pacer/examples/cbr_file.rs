//! Offline CBR shaper: read a `.ts` file, pace it to a constant-bitrate `.ts`
//! file, and print stuffing stats. Deterministic (no sockets, no wall clock), so
//! it is what the TSDuck compliance harness runs.
//!
//! ```text
//! cargo run -p ts-pacer --example cbr_file -- in.ts out.ts 12000000 [preserve|regenerate]
//! cargo run -p ts-pacer --example cbr_file -- in.ts out.ts auto        # derive the rate
//! tsp -I file out.ts -P pcrverify --jitter-max 500 -O drop   # 500 ns/us tolerance
//! ```
//!
//! The scheduler is driven by a synthetic clock derived from the source PCR, so
//! the null stuffing matches what the live egress would insert in real time.

use std::time::{Duration, Instant};

use ts_pacer::{Config, DEFAULT_AUTO_HEADROOM, Packet, PcrMode, Scheduler, TS_PACKET_SIZE, estimate_content_bitrate};

const PCR_CLOCK_HZ: u64 = 27_000_000;
const PCR_WRAP_TICKS: u64 = (1_u64 << 33) * 300;

fn main() -> std::io::Result<()> {
	let mut args = std::env::args().skip(1);
	let usage = "usage: cbr_file <in.ts> <out.ts> <bitrate_bps|auto> [preserve|regenerate]";
	let input = args.next().expect(usage);
	let output = args.next().expect(usage);
	let rate = args.next().expect(usage);
	let mode = match args.next().as_deref() {
		Some("preserve") => PcrMode::Preserve,
		Some("regenerate") | None => PcrMode::Regenerate,
		Some(other) => panic!("unknown PCR mode {other:?}; expected preserve or regenerate"),
	};

	let bytes = std::fs::read(&input)?;
	let packets: Vec<Packet> = bytes
		.chunks_exact(TS_PACKET_SIZE)
		.filter_map(|chunk| Packet::from_slice(chunk).ok())
		.collect();

	// Offline, so resolve "auto" exactly from the whole file rather than a live
	// warm-up window.
	let bitrate = if rate == "auto" {
		let content = estimate_content_bitrate(&packets).expect("need at least two PCRs to derive a rate");
		((content as f64) * (1.0 + DEFAULT_AUTO_HEADROOM)) as u64
	} else {
		rate.parse().expect("bitrate must be an integer or \"auto\"")
	};

	let config = Config::new(bitrate)
		.with_latency(Duration::ZERO)
		.with_pcr_mode(mode)
		.with_packets_per_datagram(7);
	let mut scheduler = Scheduler::new(&config);

	// Advance the synthetic clock smoothly at the source's average packet rate
	// (its PCR span spread over the packets between the first and last PCR), so
	// the offline run mirrors a real-time -re feed instead of stepping only at
	// PCR packets.
	let ticks_per_packet = average_ticks_per_packet(&packets);

	let anchor = Instant::now();
	let mut now = anchor;
	let mut out = Vec::with_capacity(bytes.len());

	for (index, packet) in packets.into_iter().enumerate() {
		now = anchor + Duration::from_nanos((index as u64 * ticks_per_packet) * 1_000_000_000 / PCR_CLOCK_HZ);
		scheduler.enqueue(packet, now);
		// Drain everything the byte clock says is due by the current media time.
		while let Some(due) = scheduler.next_due() {
			if due <= now {
				out.extend_from_slice(scheduler.emit_datagram(due));
			} else {
				break;
			}
		}
	}

	// Flush the remaining buffered tail at the mux rate.
	while scheduler.has_pending() {
		let Some(due) = scheduler.next_due() else { break };
		now = due.max(now);
		out.extend_from_slice(scheduler.emit_datagram(now));
	}

	std::fs::write(&output, &out)?;

	let stats = scheduler.stats();
	eprintln!(
		"cbr_file: {input} -> {output} | bitrate={bitrate} in_packets={} out_packets={} \
		 content={} null={} ({:.1}% stuffing) pcr_inserted={} stripped_nulls={} dropped={} rebases={}",
		bytes.len() / TS_PACKET_SIZE,
		stats.output_packets,
		stats.content_packets,
		stats.null_packets,
		stats.null_ratio() * 100.0,
		stats.pcr_inserted,
		stats.input_nulls_stripped,
		stats.dropped_packets,
		stats.pcr_rebases,
	);
	Ok(())
}

/// Forward distance on the wrapping 27 MHz PCR clock.
fn forward_delta(previous: u64, current: u64) -> u64 {
	if current >= previous {
		current - previous
	} else {
		PCR_WRAP_TICKS - previous + current
	}
}

/// Average 27 MHz ticks per packet, from the PCR span between the first and last
/// PCR-bearing packet. Falls back to a nominal ~4 Mb/s cadence if fewer than two
/// PCRs are present.
fn average_ticks_per_packet(packets: &[Packet]) -> u64 {
	let mut first: Option<(usize, u64)> = None;
	let mut last: Option<(usize, u64)> = None;
	for (index, packet) in packets.iter().enumerate() {
		if let Some(pcr) = packet.pcr() {
			if first.is_none() {
				first = Some((index, pcr));
			}
			last = Some((index, pcr));
		}
	}
	match (first, last) {
		(Some((i0, p0)), Some((i1, p1))) if i1 > i0 => forward_delta(p0, p1) / (i1 - i0) as u64,
		_ => (TS_PACKET_SIZE as u64 * 8 * PCR_CLOCK_HZ) / 4_000_000,
	}
}
