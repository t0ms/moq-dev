//! The deterministic constant-bitrate scheduler: the heart of the pacer.
//!
//! This is a pure, synchronous state machine with no I/O and no timers, so it is
//! exhaustively unit-testable by feeding it packets and synthetic [`Instant`]s.
//! The async driver in [`crate::pacer`] is a thin loop around it.
//!
//! It runs two clocks:
//!
//! - a **transport byte clock** at the configured mux rate. Output packet `n` is
//!   due at `anchor + n * 188 * 8 / bitrate`, so the wire rate is exactly
//!   constant. This clock also byte-locks the regenerated PCR.
//! - a **media clock** recovered from the source PCR. Content packets are
//!   released at the source's own media rate (estimated from PCR deltas), delayed
//!   by the configured latency, so the output preserves the source duration
//!   instead of draining a burst faster than real time.
//!
//! Every output slot emits a content packet when the media clock says one is due
//! and the buffer has it, otherwise a null packet. So a burst is absorbed by the
//! [`JitterBuffer`] and released at media rate, while the wire stays CBR.

use std::time::{Duration, Instant};

use crate::config::{Config, DEFAULT_AUTO_FALLBACK, PcrMode};
use crate::jitter_buffer::JitterBuffer;
use crate::null_insertion::{NULL_PID, null_packet, pcr_only_packet};
use crate::packet::{Packet, TS_PACKET_SIZE};
use crate::pcr::{self, PACKET_BITS, PcrRegen};
use crate::stats::Stats;

/// Smoothing factor for the media-rate EMA (weight of each new sample).
const RATE_EMA_ALPHA: f64 = 0.1;

/// Recovers the source media rate from PCR deltas and paces content release.
#[derive(Debug)]
struct MediaClock {
	/// Wall time content packet 0 becomes eligible (first enqueue + latency).
	anchor: Option<Instant>,
	/// Content packets released so far.
	released: u64,
	/// Estimated content rate in packets per second.
	rate_pps: f64,
	/// Whether `rate_pps` is a real PCR-derived estimate yet (vs the fallback).
	estimated: bool,
	/// Last source PCR observed, for delta-based rate estimation.
	last_pcr: Option<u64>,
	/// Packets enqueued since the last PCR (the numerator of a rate sample).
	packets_since_pcr: u64,
}

impl MediaClock {
	fn new(fallback_pps: f64) -> Self {
		Self {
			anchor: None,
			released: 0,
			rate_pps: fallback_pps,
			estimated: false,
			last_pcr: None,
			packets_since_pcr: 0,
		}
	}

	/// Observe an enqueued packet, refining the media-rate estimate on each PCR.
	fn observe(&mut self, packet: &Packet) {
		self.packets_since_pcr = self.packets_since_pcr.saturating_add(1);
		let Some(pcr) = packet.pcr() else {
			return;
		};
		if let Some(last) = self.last_pcr {
			let delta = pcr::forward_delta(last, pcr);
			let secs = pcr::ticks_to_duration(delta).as_secs_f64();
			// Ignore zero deltas and discontinuities (loop wrap / splice); those
			// don't reflect a real elapsed interval.
			if delta > 0 && secs > 0.0 && pcr::ticks_to_duration(delta) <= pcr::PCR_DISCONTINUITY_GAP {
				let sample = self.packets_since_pcr as f64 / secs;
				self.rate_pps = if self.estimated {
					self.rate_pps * (1.0 - RATE_EMA_ALPHA) + sample * RATE_EMA_ALPHA
				} else {
					sample
				};
				self.estimated = true;
			}
		}
		self.last_pcr = Some(pcr);
		self.packets_since_pcr = 0;
	}

	/// Number of content packets that should have been released by `now`.
	fn due(&self, now: Instant) -> u64 {
		let Some(anchor) = self.anchor else {
			return 0;
		};
		if now < anchor {
			return 0;
		}
		let elapsed = now.duration_since(anchor).as_secs_f64();
		(elapsed * self.rate_pps) as u64 + 1
	}
}

/// The CBR scheduler. See the module docs.
#[derive(Debug)]
pub struct Scheduler {
	mux_rate_bps: u64,
	latency: Duration,
	packets_per_datagram: usize,
	buffer: JitterBuffer,
	media: MediaClock,
	pcr_regen: Option<PcrRegen>,
	null: [u8; TS_PACKET_SIZE],
	/// PCR PID, learned from the first PCR-bearing packet.
	pcr_pid: Option<u16>,
	/// Continuity counter last seen on the PCR PID, for re-inserted packets.
	pcr_pid_cc: u8,
	/// Output index of the most recent PCR (content or re-inserted).
	last_pcr_index: Option<u64>,
	/// PCR re-insertion threshold, in output packets at the mux rate.
	pcr_max_packets: u64,
	/// Wall time of output packet 0 (first enqueue + latency).
	anchor: Option<Instant>,
	output_packets: u64,
	scratch: Vec<u8>,
	stats: Stats,
}

impl Scheduler {
	/// Build a scheduler from a [`Config`].
	///
	/// The scheduler runs at a fixed rate, so a [`Bitrate::Auto`](crate::Bitrate)
	/// config must be resolved to a concrete rate before it reaches here;
	/// [`crate::pace`] and [`crate::TsPacer`] do that from the source. Passed an
	/// unresolved auto config directly, it falls back to
	/// [`DEFAULT_AUTO_FALLBACK`].
	pub fn new(config: &Config) -> Self {
		let bitrate = config.resolved_bitrate().unwrap_or(DEFAULT_AUTO_FALLBACK).max(1);
		let fallback_pps = bitrate as f64 / PACKET_BITS as f64;
		let capacity = latency_to_packets(config.max_latency, bitrate).max(1);
		let pcr_regen = match config.pcr {
			PcrMode::Regenerate => Some(PcrRegen::new(bitrate)),
			PcrMode::Preserve => None,
		};
		Self {
			mux_rate_bps: bitrate,
			latency: config.latency,
			packets_per_datagram: config.packets_per_datagram.max(1),
			buffer: JitterBuffer::new(capacity),
			media: MediaClock::new(fallback_pps),
			pcr_regen,
			null: null_packet(),
			pcr_pid: None,
			pcr_pid_cc: 0,
			last_pcr_index: None,
			// Inject with margin below the hard limit so datagram granularity and
			// waiting for a stuffing slot can't push an interval past it.
			pcr_max_packets: latency_to_packets(config.pcr_max_interval.mul_f64(0.75), bitrate).max(1) as u64,
			anchor: None,
			output_packets: 0,
			scratch: Vec::with_capacity(config.packets_per_datagram.max(1) * TS_PACKET_SIZE),
			stats: Stats::default(),
		}
	}

	/// Enqueue an input packet observed at `now`. The first packet arms both
	/// clocks (output starts `latency` later, priming the buffer).
	///
	/// Incoming null/stuffing packets (PID `0x1FFF`) are stripped here: they are
	/// pure padding, carry no PID/PSI/PES structure, and the pacer inserts its
	/// own stuffing to hit the target rate. Re-pacing an already-stuffed source
	/// would otherwise double-count the source's padding as media.
	pub fn enqueue(&mut self, packet: Packet, now: Instant) {
		if packet.pid() == NULL_PID {
			self.stats.input_nulls_stripped = self.stats.input_nulls_stripped.saturating_add(1);
			return;
		}
		if self.anchor.is_none() {
			let anchor = now + self.latency;
			self.anchor = Some(anchor);
			self.media.anchor = Some(anchor);
		}
		if self.pcr_pid.is_none() && packet.has_pcr() {
			self.pcr_pid = Some(packet.pid());
		}
		self.media.observe(&packet);
		if self.buffer.push(packet) {
			self.stats.dropped_packets = self.stats.dropped_packets.saturating_add(1);
		}
	}

	/// Wall-clock instant the next output datagram is due, or `None` until the
	/// first packet has armed the clock.
	pub fn next_due(&self) -> Option<Instant> {
		self.anchor.map(|a| a + self.packets_to_duration(self.output_packets))
	}

	/// Whether any buffered content remains to be emitted.
	pub fn has_pending(&self) -> bool {
		!self.buffer.is_empty()
	}

	/// Current jitter-buffer occupancy in packets (the de-jitter cushion depth).
	pub fn buffered_packets(&self) -> usize {
		self.buffer.len()
	}

	/// A snapshot of the pacing statistics.
	pub fn stats(&self) -> Stats {
		self.stats
	}

	/// Emit one output datagram (`packets_per_datagram` transport packets) at
	/// `now`, advancing the byte clock. Returns a borrow of the internal scratch
	/// buffer, valid until the next call. Never allocates after construction.
	pub fn emit_datagram(&mut self, now: Instant) -> &[u8] {
		self.scratch.clear();
		let due = self.media.due(now);
		for _ in 0..self.packets_per_datagram {
			let index = self.output_packets;
			let want_content = self.media.released < due;
			if want_content && let Some(packet) = self.buffer.pop() {
				let is_pcr = packet.has_pcr();
				if self.pcr_pid == Some(packet.pid()) {
					self.pcr_pid_cc = packet.as_bytes()[3] & 0x0f;
				}
				let start = self.scratch.len();
				self.scratch.extend_from_slice(packet.as_bytes());
				if let Some(regen) = self.pcr_regen.as_mut() {
					let slice = &mut self.scratch[start..start + TS_PACKET_SIZE];
					if regen.rewrite(slice, index) {
						self.stats.pcr_rebases = self.stats.pcr_rebases.saturating_add(1);
					}
				}
				if is_pcr {
					self.last_pcr_index = Some(index);
				}
				self.media.released = self.media.released.saturating_add(1);
				self.stats.content_packets = self.stats.content_packets.saturating_add(1);
			} else if let Some(pcr) = self.reinsert_pcr(index) {
				// A stuffing slot doubles as a re-inserted PCR when the source's
				// own PCR would otherwise fall past the repetition limit.
				let packet = pcr_only_packet(self.pcr_pid.expect("pid set"), self.pcr_pid_cc, pcr);
				self.scratch.extend_from_slice(&packet);
				self.last_pcr_index = Some(index);
				self.stats.pcr_inserted = self.stats.pcr_inserted.saturating_add(1);
			} else {
				self.scratch.extend_from_slice(&self.null);
				self.stats.null_packets = self.stats.null_packets.saturating_add(1);
				// A null while content was due but unavailable is a genuine
				// underrun (input starved); a null while holding the cushion is
				// ordinary rate stuffing.
				if want_content {
					self.stats.underruns = self.stats.underruns.saturating_add(1);
				}
			}
			self.output_packets = self.output_packets.saturating_add(1);
			self.stats.output_packets = self.stats.output_packets.saturating_add(1);
		}
		&self.scratch
	}

	/// The byte-locked PCR to re-insert at output `index`, or `None` when
	/// re-insertion doesn't apply: preserve mode, no PCR PID learned yet, no real
	/// PCR seen yet, or the repetition limit not yet reached.
	fn reinsert_pcr(&self, index: u64) -> Option<u64> {
		let regen = self.pcr_regen.as_ref()?;
		self.pcr_pid?;
		let last = self.last_pcr_index?;
		if index.saturating_sub(last) < self.pcr_max_packets {
			return None;
		}
		regen.locked_for_index(index)
	}

	/// Wall-clock duration to transmit `packets` at the mux rate.
	fn packets_to_duration(&self, packets: u64) -> Duration {
		let nanos = u128::from(packets) * PACKET_BITS * 1_000_000_000_u128 / u128::from(self.mux_rate_bps);
		Duration::from_nanos(nanos.min(u128::from(u64::MAX)) as u64)
	}
}

/// How many 188-byte packets a `latency` window holds at `bitrate`.
fn latency_to_packets(latency: Duration, bitrate: u64) -> usize {
	let bits = latency.as_secs_f64() * bitrate as f64;
	(bits / PACKET_BITS as f64).ceil() as usize
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::config::Config;

	const MUX_RATE: u64 = 12_000_000;
	// 188 * 8 * 27_000_000 / 12_000_000 = 3384 ticks per packet.
	const TICKS_PER_PACKET: u64 = 3_384;

	fn config() -> Config {
		Config::new(MUX_RATE)
			.with_latency(Duration::ZERO)
			.with_packets_per_datagram(1)
	}

	fn content_packet(pid: u16, pcr_ticks: Option<u64>) -> Packet {
		let mut p = [0x00_u8; TS_PACKET_SIZE];
		p[0] = 0x47;
		p[1] = (pid >> 8) as u8 & 0x1f;
		p[2] = pid as u8;
		if let Some(ticks) = pcr_ticks {
			p[3] = 0x30;
			p[4] = 7;
			p[5] = 0x10;
			pcr::write_pcr(&mut p[6..12], ticks);
		} else {
			p[3] = 0x10;
		}
		Packet::from_slice(&p).unwrap()
	}

	fn pid_of(packet: &[u8]) -> u16 {
		(u16::from(packet[1] & 0x1f) << 8) | u16::from(packet[2])
	}

	fn is_null(packet: &[u8]) -> bool {
		pid_of(packet) == crate::null_insertion::NULL_PID
	}

	#[test]
	fn next_due_advances_at_the_mux_rate() {
		let mut sched = Scheduler::new(&config().with_packets_per_datagram(1));
		let t0 = Instant::now();
		assert!(sched.next_due().is_none(), "no clock before the first packet");
		sched.enqueue(content_packet(0x100, Some(0)), t0);
		let first = sched.next_due().unwrap();
		assert_eq!(first, t0, "zero latency: output packet 0 is due immediately");
		sched.emit_datagram(first);
		let second = sched.next_due().unwrap();
		// One 188-byte packet at 12 Mb/s = 188 * 8 / 12e6 s.
		let expected = Duration::from_nanos(188 * 8 * 1_000_000_000 / 12_000_000);
		assert_eq!(second.duration_since(t0), expected);
	}

	#[test]
	fn stuffs_null_when_input_is_below_rate() {
		// Media rate ~1000 pps; mux rate ~7978 pps. Most slots should be null.
		let mut sched = Scheduler::new(&config().with_packets_per_datagram(1));
		let t0 = Instant::now();
		// Two PCRs 1 ms apart carrying 1 content packet each => ~1000 pps.
		sched.enqueue(content_packet(0x100, Some(0)), t0);
		sched.enqueue(content_packet(0x100, Some(27_000)), t0);
		// Emit a datagram every packet-interval for 5 ms of output.
		let mut nulls = 0;
		let mut content = 0;
		for i in 0..100 {
			let due = sched.next_due().unwrap();
			let _ = i;
			let dg = sched.emit_datagram(due).to_vec();
			if is_null(&dg) {
				nulls += 1;
			} else {
				content += 1;
			}
		}
		assert!(
			nulls > content,
			"below-rate input should be mostly null ({nulls} null, {content} content)"
		);
		assert!(sched.stats().null_packets >= 1);
	}

	#[test]
	fn preserve_mode_leaves_pcr_untouched() {
		let cfg = config().with_pcr_mode(PcrMode::Preserve).with_packets_per_datagram(1);
		let mut sched = Scheduler::new(&cfg);
		let t0 = Instant::now();
		sched.enqueue(content_packet(0x100, Some(123_456)), t0);
		let dg = sched.emit_datagram(sched.next_due().unwrap()).to_vec();
		assert_eq!(pcr::read_pcr(&dg), Some(123_456), "preserve keeps the source PCR value");
	}

	#[test]
	fn regenerate_mode_byte_locks_pcr() {
		let cfg = config().with_pcr_mode(PcrMode::Regenerate).with_packets_per_datagram(1);
		let mut sched = Scheduler::new(&cfg);
		let t0 = Instant::now();
		// Enqueue enough content so the media clock releases several packets.
		for i in 0..8 {
			sched.enqueue(content_packet(0x100, Some(i * 90_000)), t0);
		}
		// Drain: pull datagrams well past the media schedule so all release.
		let mut pcrs = Vec::new();
		let mut now = t0;
		for _ in 0..64 {
			let dg = sched.emit_datagram(now).to_vec();
			if let Some(p) = pcr::read_pcr(&dg) {
				pcrs.push((sched.stats().output_packets - 1, p));
			}
			now += Duration::from_millis(1);
		}
		assert!(pcrs.len() >= 2, "expected multiple PCRs, got {}", pcrs.len());
		// Consecutive regenerated PCRs differ by exactly the byte distance.
		let (i0, p0) = pcrs[0];
		let (i1, p1) = pcrs[1];
		assert_eq!(
			p1 - p0,
			(i1 - i0) * TICKS_PER_PACKET,
			"PCR is byte-locked to the mux rate"
		);
	}

	#[test]
	fn reinserts_pcr_to_hold_the_repetition_limit() {
		// A source PCR far sparser than the limit forces re-insertion. With a
		// tiny limit, once the first real PCR anchors the clock every subsequent
		// stuffing slot on the PCR PID carries a byte-locked PCR.
		let cfg = config()
			.with_pcr_mode(PcrMode::Regenerate)
			.with_pcr_max_interval(Duration::from_micros(100))
			.with_packets_per_datagram(1);
		let mut sched = Scheduler::new(&cfg);
		let t0 = Instant::now();
		// One PCR packet then plain content (no further PCRs).
		sched.enqueue(content_packet(0x100, Some(0)), t0);
		for _ in 0..3 {
			sched.enqueue(content_packet(0x100, None), t0);
		}

		let mut now = t0;
		let mut pcr_packets = 0;
		let mut injected_on_pcr_pid = 0;
		for _ in 0..40 {
			let dg = sched.emit_datagram(now).to_vec();
			if pcr::read_pcr(&dg).is_some() {
				pcr_packets += 1;
				// An adaptation-only (afc=10) PCR packet on 0x100 is a re-insertion.
				if pid_of(&dg) == 0x100 && (dg[3] >> 4) & 0x03 == 0b10 {
					injected_on_pcr_pid += 1;
				}
			}
			now += Duration::from_millis(1);
		}

		assert!(sched.stats().pcr_inserted > 0, "expected PCR re-insertion");
		assert!(injected_on_pcr_pid > 0, "injected PCR must sit on the PCR PID");
		assert!(pcr_packets > 1, "output carries more PCRs than the single source PCR");
	}

	#[test]
	fn strips_input_null_packets() {
		let mut sched = Scheduler::new(&config().with_packets_per_datagram(1));
		let t0 = Instant::now();
		// A null packet (PID 0x1FFF) is stuffing: it must not be buffered as media.
		let mut null = [0x00_u8; TS_PACKET_SIZE];
		null[0] = 0x47;
		null[1] = 0x1f;
		null[2] = 0xff;
		null[3] = 0x10;
		sched.enqueue(Packet::from_slice(&null).unwrap(), t0);
		assert_eq!(sched.stats().input_nulls_stripped, 1);
		assert_eq!(sched.buffered_packets(), 0, "input nulls are not buffered");
	}

	#[test]
	fn drops_oldest_past_max_latency() {
		// Tiny max_latency => small buffer; a burst overflows and drops.
		let cfg = Config::new(MUX_RATE)
			.with_latency(Duration::ZERO)
			.with_max_latency(Duration::from_micros(200)) // ~2 packets at 12 Mb/s
			.with_packets_per_datagram(1);
		let mut sched = Scheduler::new(&cfg);
		let t0 = Instant::now();
		for _ in 0..50 {
			sched.enqueue(content_packet(0x100, None), t0);
		}
		assert!(sched.stats().dropped_packets > 0, "a burst past max_latency must drop");
	}
}
