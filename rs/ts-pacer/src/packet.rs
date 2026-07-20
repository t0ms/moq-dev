//! The [`Packet`] type: one validated, 188-byte MPEG-TS transport packet.

use bytes::Bytes;

use crate::error::{Error, Result};
use crate::pcr;

/// The fixed MPEG-TS transport packet size.
pub const TS_PACKET_SIZE: usize = 188;
/// The MPEG-TS sync byte that starts every packet.
pub const SYNC_BYTE: u8 = 0x47;

/// A single, validated 188-byte MPEG-TS transport packet.
///
/// Backed by [`Bytes`], so cloning is a cheap refcount bump and the packet path
/// stays copy-free from input to the point where a datagram is assembled. The
/// pacer never demultiplexes, remultiplexes, or rewrites PID/continuity/PSI/PES
/// content; a packet is carried through opaquely (the only exception is the six
/// PCR octets under [`PcrMode::Regenerate`](crate::PcrMode)).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Packet(Bytes);

impl Packet {
	/// Wrap an existing [`Bytes`] of exactly 188 bytes starting with the sync
	/// byte. Zero-copy: the buffer is taken as-is.
	pub fn new(bytes: Bytes) -> Result<Self> {
		if bytes.len() != TS_PACKET_SIZE {
			return Err(Error::InvalidPacket("expected exactly 188 bytes"));
		}
		if bytes[0] != SYNC_BYTE {
			return Err(Error::InvalidPacket("missing 0x47 sync byte"));
		}
		Ok(Self(bytes))
	}

	/// Copy a 188-byte slice into a new packet.
	pub fn from_slice(slice: &[u8]) -> Result<Self> {
		Self::new(Bytes::copy_from_slice(slice))
	}

	/// The 13-bit PID this packet belongs to.
	pub fn pid(&self) -> u16 {
		(u16::from(self.0[1] & 0x1f) << 8) | u16::from(self.0[2])
	}

	/// The 27 MHz PCR carried in this packet's adaptation field, if any.
	pub fn pcr(&self) -> Option<u64> {
		pcr::read_pcr(&self.0)
	}

	/// Whether this packet carries a PCR sample.
	pub fn has_pcr(&self) -> bool {
		self.pcr().is_some()
	}

	/// Borrow the raw 188 bytes.
	pub fn as_bytes(&self) -> &[u8] {
		&self.0
	}

	/// Consume the packet, returning the underlying [`Bytes`].
	pub fn into_bytes(self) -> Bytes {
		self.0
	}
}

impl AsRef<[u8]> for Packet {
	fn as_ref(&self) -> &[u8] {
		&self.0
	}
}

impl TryFrom<Bytes> for Packet {
	type Error = Error;

	fn try_from(bytes: Bytes) -> Result<Self> {
		Self::new(bytes)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn raw(pid: u16) -> [u8; TS_PACKET_SIZE] {
		let mut p = [0x00_u8; TS_PACKET_SIZE];
		p[0] = SYNC_BYTE;
		p[1] = (pid >> 8) as u8 & 0x1f;
		p[2] = pid as u8;
		p[3] = 0x10;
		p
	}

	#[test]
	fn rejects_wrong_length() {
		assert!(Packet::from_slice(&[0x47; 187]).is_err());
		assert!(Packet::from_slice(&[0x47; 189]).is_err());
	}

	#[test]
	fn rejects_missing_sync_byte() {
		let mut p = raw(0x100);
		p[0] = 0x00;
		assert!(Packet::from_slice(&p).is_err());
	}

	#[test]
	fn reads_pid() {
		let p = Packet::from_slice(&raw(0x1234 & 0x1fff)).unwrap();
		assert_eq!(p.pid(), 0x1234 & 0x1fff);
	}
}
