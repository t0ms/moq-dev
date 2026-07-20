//! Measuring the content bitrate of a run of transport packets from its PCR.

use crate::null_insertion::NULL_PID;
use crate::packet::{Packet, TS_PACKET_SIZE};
use crate::pcr::{PCR_CLOCK_HZ, forward_delta};

/// Estimate the media/content bitrate of `packets`, in bits per second, from
/// their PCR clock.
///
/// It divides the content bytes carried between the first and last PCR sample by
/// the PCR time they span. Null/stuffing packets (PID `0x1FFF`) are excluded, so
/// the result is the true *content* rate, not a padded mux rate. This is what
/// [`Bitrate::Auto`](crate::Bitrate::Auto) uses to self-configure. Returns
/// `None` when the run has fewer than two PCR samples or a zero span.
pub fn estimate_content_bitrate(packets: &[Packet]) -> Option<u64> {
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

	let (first_index, first_pcr) = first?;
	let (last_index, last_pcr) = last?;
	if last_index <= first_index {
		return None;
	}
	let span_ticks = forward_delta(first_pcr, last_pcr);
	if span_ticks == 0 {
		return None;
	}

	let content = packets[first_index..=last_index]
		.iter()
		.filter(|packet| packet.pid() != NULL_PID)
		.count() as u128;
	let bits = content * (TS_PACKET_SIZE as u128) * 8;
	let bps = bits * u128::from(PCR_CLOCK_HZ) / u128::from(span_ticks);
	Some(bps.min(u128::from(u64::MAX)) as u64)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::pcr::{TS_PACKET_SIZE as PKT, write_pcr};

	fn pcr_packet(pcr_ticks: u64) -> Packet {
		let mut p = [0xff_u8; PKT];
		p[0] = 0x47;
		p[1] = 0x01;
		p[2] = 0x00;
		p[3] = 0x30;
		p[4] = 7;
		p[5] = 0x10;
		write_pcr(&mut p[6..12], pcr_ticks);
		Packet::from_slice(&p).unwrap()
	}

	fn plain(pid: u16) -> Packet {
		let mut p = [0x00_u8; PKT];
		p[0] = 0x47;
		p[1] = (pid >> 8) as u8 & 0x1f;
		p[2] = pid as u8;
		p[3] = 0x10;
		Packet::from_slice(&p).unwrap()
	}

	#[test]
	fn none_without_two_pcrs() {
		assert_eq!(estimate_content_bitrate(&[]), None);
		assert_eq!(estimate_content_bitrate(&[pcr_packet(0)]), None);
		assert_eq!(estimate_content_bitrate(&[plain(0x100), plain(0x100)]), None);
	}

	#[test]
	fn measures_content_rate_between_pcrs() {
		// 10 content packets across a 1/25 s (40 ms) PCR span. 10 * 188 * 8 bits
		// over 0.04 s = 376_000 bps.
		let mut packets = vec![pcr_packet(0)];
		for _ in 0..8 {
			packets.push(plain(0x100));
		}
		packets.push(pcr_packet(PCR_CLOCK_HZ / 25));
		assert_eq!(estimate_content_bitrate(&packets), Some(376_000));
	}

	#[test]
	fn excludes_null_padding() {
		let mut with_nulls = vec![pcr_packet(0)];
		for _ in 0..8 {
			with_nulls.push(plain(0x100));
		}
		for _ in 0..100 {
			with_nulls.push(plain(NULL_PID));
		}
		with_nulls.push(pcr_packet(PCR_CLOCK_HZ / 25));
		// Same content rate as without the padding.
		assert_eq!(estimate_content_bitrate(&with_nulls), Some(376_000));
	}
}
