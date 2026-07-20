//! Null-packet (stuffing) generation.
//!
//! When the input rate is below the target output rate, the scheduler emits null
//! packets so the transport byte clock still runs at exactly the configured
//! bitrate. Null packets carry PID `0x1FFF` and a payload of `0xFF`; their
//! continuity counter is don't-care per ISO/IEC 13818-1 (and ignored by
//! TR 101 290), so a single shared template is reused for every stuffing packet.

use crate::pcr::{self, TS_PACKET_SIZE};

/// The null / stuffing PID.
pub const NULL_PID: u16 = 0x1fff;

/// An adaptation-only packet on `pid` carrying `pcr_ticks` and nothing else, for
/// PCR re-insertion. Adaptation field control `10` (no payload) means the
/// continuity counter must not advance, so `cc` is set to the PID's last
/// transmitted value; the rest of the packet is `0xFF` stuffing.
pub fn pcr_only_packet(pid: u16, cc: u8, pcr_ticks: u64) -> [u8; TS_PACKET_SIZE] {
	let mut packet = [0xff_u8; TS_PACKET_SIZE];
	packet[0] = 0x47;
	packet[1] = (pid >> 8) as u8 & 0x1f;
	packet[2] = pid as u8;
	packet[3] = 0x20 | (cc & 0x0f); // adaptation field only, no payload
	packet[4] = (TS_PACKET_SIZE - 5) as u8; // adaptation field fills the packet
	packet[5] = 0x10; // PCR flag set
	pcr::write_pcr(&mut packet[6..12], pcr_ticks);
	packet
}

/// A standard MPEG-TS null packet: sync byte `0x47`, PID `0x1FFF`, payload-only
/// (adaptation field control `01`), continuity counter `0`, `0xFF` payload.
pub const fn null_packet() -> [u8; TS_PACKET_SIZE] {
	let mut packet = [0xff_u8; TS_PACKET_SIZE];
	packet[0] = 0x47;
	packet[1] = 0x1f;
	packet[2] = 0xff;
	packet[3] = 0x10;
	packet
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn null_packet_has_the_null_pid() {
		let p = null_packet();
		assert_eq!(p[0], 0x47);
		let pid = (u16::from(p[1] & 0x1f) << 8) | u16::from(p[2]);
		assert_eq!(pid, NULL_PID);
		assert_eq!(p.len(), TS_PACKET_SIZE);
	}
}
