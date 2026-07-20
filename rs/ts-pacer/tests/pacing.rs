//! End-to-end pacing tests over the async engine.
//!
//! These drive the real [`ts_pacer::pace`] loop under `tokio`'s paused clock, so
//! they are deterministic (virtual time auto-advances to each scheduled emit)
//! yet exercise the full source -> scheduler -> sink path.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ts_pacer::{Config, IterSource, Packet, PcrMode, Sink, TS_PACKET_SIZE, pace};

const MUX_RATE: u64 = 10_000_000;
const MEDIA_PID: u16 = 0x100;
// 188 * 8 * 27_000_000 / 10_000_000 = 4060.8; not integer, so PCR steps are
// checked as byte-proportional rather than a fixed constant.

/// A `Sink` that appends every datagram to a shared buffer.
struct CollectSink(Arc<Mutex<Vec<u8>>>);

impl Sink for CollectSink {
	async fn send(&mut self, datagram: &[u8]) -> ts_pacer::Result<()> {
		self.0.lock().unwrap().extend_from_slice(datagram);
		Ok(())
	}
}

/// A content packet on `MEDIA_PID`, optionally carrying a PCR.
fn packet(pcr_ticks: Option<u64>, cc: u8) -> Packet {
	let mut p = [0x00_u8; TS_PACKET_SIZE];
	p[0] = 0x47;
	p[1] = (MEDIA_PID >> 8) as u8 & 0x1f;
	p[2] = MEDIA_PID as u8;
	match pcr_ticks {
		Some(ticks) => {
			p[3] = 0x30 | (cc & 0x0f); // adaptation + payload
			p[4] = 7;
			p[5] = 0x10;
			write_pcr(&mut p[6..12], ticks);
		}
		None => p[3] = 0x10 | (cc & 0x0f),
	}
	Packet::from_slice(&p).unwrap()
}

fn write_pcr(target: &mut [u8], pcr_ticks: u64) {
	let base = (pcr_ticks / 300) & ((1_u64 << 33) - 1);
	let ext = pcr_ticks % 300;
	target[0] = (base >> 25) as u8;
	target[1] = (base >> 17) as u8;
	target[2] = (base >> 9) as u8;
	target[3] = (base >> 1) as u8;
	target[4] = ((base & 0x01) as u8) << 7 | 0x7e | ((ext >> 8) as u8 & 0x01);
	target[5] = ext as u8;
}

fn read_pcr(packet: &[u8]) -> Option<u64> {
	if packet[3] >> 4 & 0x03 != 0b11 || packet[4] < 7 || packet[5] & 0x10 == 0 {
		return None;
	}
	let pcr = &packet[6..12];
	let base = (u64::from(pcr[0]) << 25)
		| (u64::from(pcr[1]) << 17)
		| (u64::from(pcr[2]) << 9)
		| (u64::from(pcr[3]) << 1)
		| (u64::from(pcr[4]) >> 7);
	let ext = (u64::from(pcr[4] & 0x01) << 8) | u64::from(pcr[5]);
	Some(base * 300 + ext)
}

fn pid(packet: &[u8]) -> u16 {
	(u16::from(packet[1] & 0x1f) << 8) | u16::from(packet[2])
}

/// Build a ~5 Mb/s media stream of `count` packets, a PCR every `pcr_every`.
fn media_stream(count: usize, media_rate_bps: u64, pcr_every: usize) -> Vec<Packet> {
	let ticks_per_packet = (TS_PACKET_SIZE as u64 * 8 * 27_000_000) / media_rate_bps;
	(0..count)
		.map(|i| {
			let cc = (i % 16) as u8;
			if i % pcr_every == 0 {
				packet(Some(i as u64 * ticks_per_packet), cc)
			} else {
				packet(None, cc)
			}
		})
		.collect()
}

#[tokio::test(start_paused = true)]
async fn produces_constant_bitrate_output_with_null_stuffing() {
	let input = media_stream(700, 5_000_000, 7);
	let content_in = input.len() as u64;
	let out = Arc::new(Mutex::new(Vec::new()));

	let config = Config::new(MUX_RATE)
		.with_latency(Duration::from_millis(50))
		.with_pcr_mode(PcrMode::Regenerate);
	let stats = pace(config, IterSource::new(input), CollectSink(out.clone()))
		.await
		.unwrap();

	// Every content packet is released (none dropped): duration is preserved.
	assert_eq!(stats.content_packets, content_in, "all content forwarded");
	assert_eq!(stats.dropped_packets, 0, "no drops below max_latency");
	// Mux rate (10 Mb/s) is double the media rate (5 Mb/s), so ~half is null.
	assert!(stats.null_packets > 0, "below-rate input must be null-stuffed");
	assert!(
		stats.output_packets >= content_in,
		"output ({}) carries content plus stuffing",
		stats.output_packets
	);

	let bytes = out.lock().unwrap().clone();
	assert_eq!(bytes.len() % TS_PACKET_SIZE, 0, "whole packets only");

	// Structural: every emitted packet is a valid 188-byte packet.
	let mut content = 0u64;
	let mut nulls = 0u64;
	let mut pcrs: Vec<(u64, u64)> = Vec::new();
	for (index, chunk) in bytes.chunks_exact(TS_PACKET_SIZE).enumerate() {
		assert_eq!(chunk[0], 0x47, "sync byte at packet {index}");
		if pid(chunk) == ts_pacer::NULL_PID {
			nulls += 1;
		} else {
			content += 1;
			if let Some(p) = read_pcr(chunk) {
				pcrs.push((index as u64, p));
			}
		}
	}
	assert_eq!(content, content_in, "content count matches input");
	assert_eq!(nulls, stats.null_packets);

	// Regenerated PCR is byte-locked: the PCR delta equals the output byte
	// distance clocked at the mux rate (within one tick of integer rounding).
	assert!(pcrs.len() >= 3, "expected several PCRs, got {}", pcrs.len());
	for pair in pcrs.windows(2) {
		let (i0, p0) = pair[0];
		let (i1, p1) = pair[1];
		let expected = ((i1 - i0) as u128 * TS_PACKET_SIZE as u128 * 8 * 27_000_000 / MUX_RATE as u128) as u64;
		let actual = p1 - p0;
		let diff = actual.abs_diff(expected);
		assert!(diff <= 2, "PCR not byte-locked: got {actual}, expected ~{expected}");
	}
}

#[tokio::test(start_paused = true)]
async fn preserve_mode_keeps_source_pcr_values() {
	let input = media_stream(200, 5_000_000, 7);
	let expected_first_pcr = input.iter().find_map(|p| p.pcr());
	let out = Arc::new(Mutex::new(Vec::new()));

	let config = Config::new(MUX_RATE)
		.with_latency(Duration::from_millis(20))
		.with_pcr_mode(PcrMode::Preserve);
	pace(config, IterSource::new(input), CollectSink(out.clone()))
		.await
		.unwrap();

	let bytes = out.lock().unwrap().clone();
	let first_pcr = bytes.chunks_exact(TS_PACKET_SIZE).find_map(read_pcr);
	assert_eq!(
		first_pcr, expected_first_pcr,
		"preserve keeps source PCR values verbatim"
	);
}

#[tokio::test(start_paused = true)]
async fn auto_rate_matches_source_content_rate() {
	// A steady 5 Mb/s media stream. Auto should lock the output near the content
	// rate plus the default 15% headroom (~5.75 Mb/s), so only a little stuffing.
	let input = media_stream(700, 5_000_000, 7);
	let content_in = input.len() as u64;
	let out = Arc::new(Mutex::new(Vec::new()));

	let config = Config::auto().with_latency(Duration::from_millis(50));
	let stats = pace(config, IterSource::new(input), CollectSink(out.clone()))
		.await
		.unwrap();

	assert_eq!(stats.content_packets, content_in, "all content forwarded");
	assert_eq!(stats.dropped_packets, 0, "self-tuned rate must not overflow");
	// Near the content rate: far less stuffing than the 2x-overprovisioned
	// explicit-rate test, and nowhere near the 4 Mb/s no-PCR fallback.
	let ratio = stats.null_ratio();
	assert!(
		(0.02..0.30).contains(&ratio),
		"auto rate should track content + headroom, got {:.1}% stuffing",
		ratio * 100.0
	);
}

#[tokio::test(start_paused = true)]
async fn empty_source_produces_no_output() {
	let out = Arc::new(Mutex::new(Vec::new()));
	let stats = pace(
		Config::new(MUX_RATE),
		IterSource::new(Vec::<Packet>::new()),
		CollectSink(out.clone()),
	)
	.await
	.unwrap();
	assert_eq!(stats.output_packets, 0);
	assert!(out.lock().unwrap().is_empty());
}
