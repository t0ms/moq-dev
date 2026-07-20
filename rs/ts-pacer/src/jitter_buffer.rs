//! A bounded FIFO that absorbs input burst.
//!
//! MoQ (and every other object/datagram transport) delivers transport packets in
//! bursts. The jitter buffer holds them until the scheduler releases them on the
//! output byte clock, smoothing the burst into a steady stream. It is bounded: a
//! sustained input rate above the configured bitrate can't grow it without limit,
//! it drops the oldest packet instead, which keeps both latency and memory
//! bounded (the broadcast trade-off: shed the stalest media, hold the clock).

use std::collections::VecDeque;

use crate::packet::Packet;

/// A bounded packet FIFO. See the module docs.
#[derive(Debug)]
pub struct JitterBuffer {
	queue: VecDeque<Packet>,
	capacity: usize,
}

impl JitterBuffer {
	/// Create a buffer holding at most `capacity` packets (clamped to at least 1).
	pub fn new(capacity: usize) -> Self {
		Self {
			queue: VecDeque::new(),
			capacity: capacity.max(1),
		}
	}

	/// Push a packet. Returns `true` if the buffer was full and the oldest packet
	/// was dropped to make room (the caller bumps the drop stat).
	pub fn push(&mut self, packet: Packet) -> bool {
		let dropped = if self.queue.len() >= self.capacity {
			self.queue.pop_front();
			true
		} else {
			false
		};
		self.queue.push_back(packet);
		dropped
	}

	/// Remove and return the oldest packet, if any.
	pub fn pop(&mut self) -> Option<Packet> {
		self.queue.pop_front()
	}

	/// Current occupancy in packets.
	pub fn len(&self) -> usize {
		self.queue.len()
	}

	/// Whether the buffer is empty.
	pub fn is_empty(&self) -> bool {
		self.queue.is_empty()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::packet::TS_PACKET_SIZE;

	fn packet(marker: u8) -> Packet {
		let mut p = [0x00_u8; TS_PACKET_SIZE];
		p[0] = 0x47;
		p[3] = 0x10;
		p[4] = marker;
		Packet::from_slice(&p).unwrap()
	}

	#[test]
	fn fifo_order() {
		let mut buf = JitterBuffer::new(4);
		assert!(!buf.push(packet(1)));
		assert!(!buf.push(packet(2)));
		assert_eq!(buf.len(), 2);
		assert_eq!(buf.pop().unwrap().as_bytes()[4], 1);
		assert_eq!(buf.pop().unwrap().as_bytes()[4], 2);
		assert!(buf.is_empty());
	}

	#[test]
	fn drops_oldest_when_full() {
		let mut buf = JitterBuffer::new(2);
		assert!(!buf.push(packet(1)));
		assert!(!buf.push(packet(2)));
		assert!(buf.push(packet(3)), "third push drops the oldest");
		assert_eq!(buf.len(), 2);
		// Oldest (1) was dropped; 2 then 3 remain.
		assert_eq!(buf.pop().unwrap().as_bytes()[4], 2);
		assert_eq!(buf.pop().unwrap().as_bytes()[4], 3);
	}
}
