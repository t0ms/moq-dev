//! PCR (Program Clock Reference) reading, writing, and byte-locked regeneration.
//!
//! The MPEG-TS PCR runs on a 27 MHz clock, carried in a packet's adaptation
//! field as a 33-bit base (90 kHz) plus a 9-bit extension (300 counts per base
//! tick). These helpers operate on raw 188-byte packets and never allocate.

use std::time::Duration;

/// TS packet size in bytes.
pub const TS_PACKET_SIZE: usize = 188;
/// The PCR system clock frequency in Hz.
pub const PCR_CLOCK_HZ: u64 = 27_000_000;
/// The full 27 MHz PCR value wraps here (the 33-bit base is at 90 kHz).
pub const PCR_WRAP_TICKS: u64 = (1_u64 << 33) * 300;
/// Bits carried by one 188-byte transport packet.
pub const PACKET_BITS: u128 = (TS_PACKET_SIZE as u128) * 8;

/// A source PCR forward jump larger than this is treated as a discontinuity
/// (loop wrap / splice) rather than a real elapsed interval.
pub const PCR_DISCONTINUITY_GAP: Duration = Duration::from_secs(5);

/// Read the 27 MHz PCR from a packet's adaptation field, if it carries one.
pub fn read_pcr(packet: &[u8]) -> Option<u64> {
	if packet.len() != TS_PACKET_SIZE || packet[0] != 0x47 {
		return None;
	}
	let afc = (packet[3] >> 4) & 0x03;
	if !matches!(afc, 0b10 | 0b11) {
		return None;
	}
	let af_len = usize::from(packet[4]);
	// Need the flags octet plus the six PCR octets.
	if af_len < 7 || 5 + af_len > packet.len() {
		return None;
	}
	if packet[5] & 0x10 == 0 {
		return None; // PCR flag clear
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

/// Write a 27 MHz PCR value into the six PCR octets (the six bytes immediately
/// after the adaptation-field flags, i.e. `packet[6..12]`). The six reserved
/// bits are set to `1` as ISO/IEC 13818-1 requires.
pub fn write_pcr(target: &mut [u8], pcr_ticks: u64) {
	debug_assert!(target.len() >= 6, "PCR field must be at least six octets");
	let base = (pcr_ticks / 300) & ((1_u64 << 33) - 1);
	let ext = pcr_ticks % 300;
	target[0] = (base >> 25) as u8;
	target[1] = (base >> 17) as u8;
	target[2] = (base >> 9) as u8;
	target[3] = (base >> 1) as u8;
	target[4] = ((base & 0x01) as u8) << 7 | 0x7e | ((ext >> 8) as u8 & 0x01);
	target[5] = ext as u8;
}

/// Set the adaptation-field discontinuity_indicator bit on a PCR-bearing packet,
/// signalling that its PCR is not continuous with the PID's previous packet.
/// Returns `true` when the bit was set (the packet has an adaptation field).
pub fn set_discontinuity_indicator(packet: &mut [u8]) -> bool {
	if packet.len() != TS_PACKET_SIZE || packet[0] != 0x47 {
		return false;
	}
	let afc = (packet[3] >> 4) & 0x03;
	if !matches!(afc, 0b10 | 0b11) || packet[4] == 0 {
		return false;
	}
	packet[5] |= 0x80;
	true
}

/// Forward distance from `previous` to `current` on the wrapping PCR clock.
pub fn forward_delta(previous: u64, current: u64) -> u64 {
	if current >= previous {
		current - previous
	} else {
		PCR_WRAP_TICKS - previous + current
	}
}

/// Convert a count of 27 MHz PCR ticks to a wall-clock duration.
pub fn ticks_to_duration(ticks: u64) -> Duration {
	let nanos = u128::from(ticks) * 1_000_000_000_u128 / u128::from(PCR_CLOCK_HZ);
	Duration::from_nanos(nanos.min(u128::from(u64::MAX)) as u64)
}

/// Byte-locked PCR regenerator (used by [`PcrMode::Regenerate`](crate::PcrMode)).
///
/// Given a target mux rate, it maps every PCR-bearing packet's *output* byte
/// position to a PCR value, so the emitted PCR tracks a perfectly constant byte
/// clock: `PCR == byte_offset * 8 * 27_000_000 / mux_rate`. It re-bases that
/// mapping on the first PCR and on a genuine source discontinuity (so the
/// PTS<->PCR relationship, and therefore the decoder buffer model, is preserved
/// across a splice or loop wrap), flagging the re-based packet with the
/// discontinuity indicator.
#[derive(Clone, Copy, Debug)]
pub struct PcrRegen {
	mux_rate_bps: u64,
	/// `(output_packet_index, pcr_value)` of the most recent re-base.
	anchor: Option<(u64, u64)>,
	/// Last source PCR observed, used to detect discontinuities.
	last_src: Option<u64>,
}

impl PcrRegen {
	/// Create a regenerator for the given target mux rate (bits per second).
	pub fn new(mux_rate_bps: u64) -> Self {
		Self {
			mux_rate_bps: mux_rate_bps.max(1),
			anchor: None,
			last_src: None,
		}
	}

	/// Rewrite the PCR carried by `packet` (which sits at output byte position
	/// `output_index`) to its byte-locked value, in place. A no-op on a packet
	/// with no PCR. Returns `true` if this packet re-based the anchor (a genuine
	/// source discontinuity), so the caller can bump the re-base stat; the
	/// discontinuity indicator is set here regardless.
	pub fn rewrite(&mut self, packet: &mut [u8], output_index: u64) -> bool {
		let Some(src) = read_pcr(packet) else {
			return false;
		};

		let discontinuity = match self.last_src {
			None => false,
			Some(last) => {
				let delta = forward_delta(last, src);
				delta == 0 || ticks_to_duration(delta) > PCR_DISCONTINUITY_GAP
			}
		};
		self.last_src = Some(src);

		let mut rebased = false;
		match self.anchor {
			Some(_) if !discontinuity => {}
			_ => {
				rebased = self.anchor.is_some();
				self.anchor = Some((output_index, src % PCR_WRAP_TICKS));
			}
		}

		let (anchor_index, anchor_pcr) = self.anchor.expect("anchor set above");
		let delta_packets = u128::from(output_index.saturating_sub(anchor_index));
		let ticks = (delta_packets * PACKET_BITS * u128::from(PCR_CLOCK_HZ) / u128::from(self.mux_rate_bps)) as u64;
		let pcr_out = (anchor_pcr + ticks) % PCR_WRAP_TICKS;

		write_pcr(&mut packet[6..12], pcr_out);
		set_discontinuity_indicator_if(packet, rebased);
		rebased
	}

	/// The byte-locked PCR for a synthetic packet at `output_index`, using the
	/// current anchor without disturbing the regenerator's state. Used for PCR
	/// re-insertion. `None` before the first real PCR has set the anchor.
	pub fn locked_for_index(&self, output_index: u64) -> Option<u64> {
		let (anchor_index, anchor_pcr) = self.anchor?;
		let delta_packets = u128::from(output_index.saturating_sub(anchor_index));
		let ticks = (delta_packets * PACKET_BITS * u128::from(PCR_CLOCK_HZ) / u128::from(self.mux_rate_bps)) as u64;
		Some((anchor_pcr + ticks) % PCR_WRAP_TICKS)
	}
}

/// Set the discontinuity indicator only when `flag` is true.
fn set_discontinuity_indicator_if(packet: &mut [u8], flag: bool) {
	if flag {
		set_discontinuity_indicator(packet);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn pcr_packet(pcr_ticks: u64) -> [u8; TS_PACKET_SIZE] {
		let mut p = [0xff_u8; TS_PACKET_SIZE];
		p[0] = 0x47;
		p[1] = 0x01;
		p[2] = 0x00;
		p[3] = 0x30; // adaptation + payload
		p[4] = 7; // adaptation field length
		p[5] = 0x10; // PCR flag, discontinuity clear
		write_pcr(&mut p[6..12], pcr_ticks);
		p
	}

	#[test]
	fn round_trips_pcr_value() {
		for ticks in [0, 1, 300, 900_000, PCR_CLOCK_HZ, PCR_WRAP_TICKS - 1] {
			let p = pcr_packet(ticks);
			assert_eq!(read_pcr(&p), Some(ticks), "ticks={ticks}");
		}
	}

	#[test]
	fn no_pcr_when_flag_absent() {
		let mut p = pcr_packet(123);
		p[5] = 0x00; // clear PCR flag
		assert_eq!(read_pcr(&p), None);
	}

	#[test]
	fn forward_delta_handles_wrap() {
		assert_eq!(forward_delta(PCR_WRAP_TICKS - 10, 20), 30);
		assert_eq!(forward_delta(5, 25), 20);
	}

	#[test]
	fn byte_locks_pcr_to_the_mux_rate() {
		// 188 * 8 * 27_000_000 / 12_000_000 = 3384 ticks per packet (exact).
		let mut regen = PcrRegen::new(12_000_000);
		let mut a = pcr_packet(100_000);
		assert!(!regen.rewrite(&mut a, 0));
		assert_eq!(read_pcr(&a), Some(100_000), "first PCR keeps its source value");

		let mut b = pcr_packet(999_999);
		assert!(!regen.rewrite(&mut b, 7));
		assert_eq!(
			read_pcr(&b).unwrap() - read_pcr(&a).unwrap(),
			7 * 3_384,
			"PCR advances by exactly the byte distance at the mux rate"
		);
	}

	#[test]
	fn rebases_and_flags_on_source_discontinuity() {
		let mut regen = PcrRegen::new(12_000_000);
		let mut a = pcr_packet(PCR_CLOCK_HZ); // t = 1 s
		regen.rewrite(&mut a, 0);
		assert!(a[5] & 0x80 == 0, "stream start is not a discontinuity");

		// A large backward jump (loop wrap) re-bases to the new source epoch and
		// flags the discontinuity indicator.
		let mut b = pcr_packet(123_456);
		assert!(regen.rewrite(&mut b, 7));
		assert_eq!(read_pcr(&b), Some(123_456), "PCR re-bases to the new source value");
		assert!(b[5] & 0x80 != 0, "re-base sets the discontinuity indicator");
	}

	#[test]
	fn rewrite_preserves_non_pcr_bytes() {
		let mut regen = PcrRegen::new(12_000_000);
		let original = pcr_packet(555);
		let mut p = original;
		regen.rewrite(&mut p, 3);
		for (i, (before, after)) in original.iter().zip(p.iter()).enumerate() {
			// Only the flags octet (discontinuity bit) and the six PCR octets may change.
			if (5..12).contains(&i) {
				continue;
			}
			assert_eq!(before, after, "non-PCR byte {i} must be unchanged");
		}
	}
}
