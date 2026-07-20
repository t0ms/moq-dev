//! Packet [`Source`]s: where the pacer pulls its input from.
//!
//! The pacer is built around a source/sink pipeline so that any producer of
//! MPEG-TS packets is just one `Source` implementation. A MoQ subscriber, an SRT
//! or RIST receiver, a file reader, or an in-memory test vector all look
//! identical to the engine. Nothing here knows what produced the packets.

use std::future::Future;

use tokio::io::{AsyncRead, AsyncReadExt};

use crate::error::Result;
use crate::packet::{Packet, TS_PACKET_SIZE};

/// An asynchronous source of MPEG-TS packets.
///
/// [`recv`](Source::recv) yields the next packet, or `Ok(None)` once the source
/// is exhausted (which tells the pacer to flush its buffer and finish). The
/// returned future is `Send` so the pacer can run on a spawned task.
pub trait Source {
	/// Pull the next packet, or `Ok(None)` at end of input.
	fn recv(&mut self) -> impl Future<Output = Result<Option<Packet>>> + Send;
}

/// A [`Source`] over any [`AsyncRead`], reading fixed 188-byte packets.
///
/// This is the generic byte-stream adapter: point it at a socket, a pipe, a
/// file, or process stdin (e.g. the `moq ... export ts` output) and it hands
/// whole transport packets to the pacer. It resynchronises on the `0x47` sync
/// byte if the stream slips, and discards a trailing partial packet at EOF.
pub struct ReadSource<R> {
	reader: R,
	buf: [u8; TS_PACKET_SIZE],
	filled: usize,
}

impl<R: AsyncRead + Unpin + Send> ReadSource<R> {
	/// Wrap an [`AsyncRead`] as a packet source.
	pub fn new(reader: R) -> Self {
		Self {
			reader,
			buf: [0; TS_PACKET_SIZE],
			filled: 0,
		}
	}
}

impl<R: AsyncRead + Unpin + Send> Source for ReadSource<R> {
	async fn recv(&mut self) -> Result<Option<Packet>> {
		loop {
			while self.filled < TS_PACKET_SIZE {
				let n = self.reader.read(&mut self.buf[self.filled..]).await?;
				if n == 0 {
					return Ok(None); // EOF; any partial packet is discarded.
				}
				self.filled += n;
			}
			if self.buf[0] != crate::packet::SYNC_BYTE {
				// Slipped alignment: drop one byte and refill.
				self.buf.copy_within(1.., 0);
				self.filled = TS_PACKET_SIZE - 1;
				continue;
			}
			self.filled = 0;
			// Already length- and sync-validated, so this can't fail.
			return Ok(Some(Packet::from_slice(&self.buf)?));
		}
	}
}

/// A [`Source`] that yields packets from an in-memory collection, for tests and
/// offline tooling. Returns `None` once drained.
pub struct IterSource<I> {
	iter: I,
}

impl<I> IterSource<I>
where
	I: Iterator<Item = Packet> + Send,
{
	/// Create a source over anything iterable into packets.
	pub fn new<T: IntoIterator<IntoIter = I>>(packets: T) -> Self {
		Self {
			iter: packets.into_iter(),
		}
	}
}

impl<I> Source for IterSource<I>
where
	I: Iterator<Item = Packet> + Send,
{
	async fn recv(&mut self) -> Result<Option<Packet>> {
		Ok(self.iter.next())
	}
}
