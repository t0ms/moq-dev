//! Output [`Sink`]s: where paced datagrams go.
//!
//! The scheduler produces coalesced datagrams (7 * 188 = 1316 bytes by default);
//! a `Sink` decides how they leave the process. Built-in sinks cover a raw
//! byte-stream writer (a pipe/file/stdout), raw UDP, RTP-encapsulated MPEG-TS
//! (RFC 2250), and a caller-supplied callback for embedding. Implement the trait
//! for anything else (an ST 2022-7 pair, an FEC-protected path) without touching
//! the pacing engine.

use std::future::Future;
use std::net::SocketAddr;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::net::UdpSocket;

use crate::error::Result;

/// An asynchronous sink for paced MPEG-TS datagrams.
///
/// Each call receives one datagram (a whole number of 188-byte packets). The
/// returned future is `Send` so the pacer can run on a spawned task.
pub trait Sink {
	/// Send one datagram of transport packets.
	fn send(&mut self, datagram: &[u8]) -> impl Future<Output = Result<()>> + Send;
}

/// A [`Sink`] over any [`AsyncWrite`], writing the raw transport bytes.
///
/// The byte-stream counterpart to [`ReadSource`](crate::ReadSource): point it at
/// a file, a socket, or process stdout to pipe the paced stream onward, exactly
/// as a MoQ subscriber pipes `export ts` (e.g. `moq ... export ts | pacer |
/// ffplay -i -`). A stdout pipe carries no RTP sequence numbers, so it sidesteps
/// the receiver-side RTP reorder buffer that [`RtpSink`] can trip on a restart.
///
/// Each datagram is flushed after writing so a downstream reader sees it without
/// extra buffering latency.
pub struct WriteSink<W> {
	writer: W,
}

impl<W: AsyncWrite + Unpin + Send> WriteSink<W> {
	/// Wrap an [`AsyncWrite`] (a pipe, file, or `tokio::io::stdout()`) as a sink.
	pub fn new(writer: W) -> Self {
		Self { writer }
	}
}

impl<W: AsyncWrite + Unpin + Send> Sink for WriteSink<W> {
	async fn send(&mut self, datagram: &[u8]) -> Result<()> {
		self.writer.write_all(datagram).await?;
		self.writer.flush().await?;
		Ok(())
	}
}

/// Sends each datagram as a raw UDP payload to a fixed destination (unicast or
/// multicast). The socket is provided by the caller, so binding, multicast TTL,
/// interface selection, and connect are all the caller's policy.
pub struct UdpSink {
	socket: UdpSocket,
	destination: SocketAddr,
}

impl UdpSink {
	/// Send datagrams from `socket` to `destination`.
	pub fn new(socket: UdpSocket, destination: SocketAddr) -> Self {
		Self { socket, destination }
	}
}

impl Sink for UdpSink {
	async fn send(&mut self, datagram: &[u8]) -> Result<()> {
		self.socket.send_to(datagram, self.destination).await?;
		Ok(())
	}
}

/// RTP payload type for MPEG-2 transport streams (RFC 2250 / RFC 3551).
pub const RTP_PAYLOAD_TYPE_MP2T: u8 = 33;
const RTP_HEADER_SIZE: usize = 12;
const RTP_VERSION: u8 = 2;

/// Wraps each datagram in an RTP header (RFC 2250 MPEG-TS carriage) and sends it
/// over UDP. Sequence numbers increment per datagram and wrap; the 90 kHz
/// timestamp is sampled from a monotonic start instant.
pub struct RtpSink {
	socket: UdpSocket,
	destination: SocketAddr,
	sequence: u16,
	ssrc: u32,
	started_at: Instant,
	scratch: Vec<u8>,
}

impl RtpSink {
	/// Send RTP/MP2T datagrams from `socket` to `destination` with a random SSRC.
	pub fn new(socket: UdpSocket, destination: SocketAddr) -> Self {
		Self::with_ssrc(socket, destination, generate_ssrc())
	}

	/// Send RTP/MP2T datagrams with a fixed SSRC (e.g. for an ST 2022-7 pair).
	pub fn with_ssrc(socket: UdpSocket, destination: SocketAddr, ssrc: u32) -> Self {
		Self {
			socket,
			destination,
			sequence: 0,
			ssrc,
			started_at: Instant::now(),
			scratch: Vec::with_capacity(RTP_HEADER_SIZE + 1316),
		}
	}
}

impl Sink for RtpSink {
	async fn send(&mut self, datagram: &[u8]) -> Result<()> {
		let sequence = self.sequence;
		self.sequence = self.sequence.wrapping_add(1);
		let timestamp = rtp_timestamp_90khz(self.started_at);

		self.scratch.clear();
		self.scratch.extend_from_slice(&[0; RTP_HEADER_SIZE]);
		let header = &mut self.scratch[..RTP_HEADER_SIZE];
		header[0] = RTP_VERSION << 6;
		header[1] = RTP_PAYLOAD_TYPE_MP2T & 0x7f;
		header[2..4].copy_from_slice(&sequence.to_be_bytes());
		header[4..8].copy_from_slice(&timestamp.to_be_bytes());
		header[8..12].copy_from_slice(&self.ssrc.to_be_bytes());
		self.scratch.extend_from_slice(datagram);

		self.socket.send_to(&self.scratch, self.destination).await?;
		Ok(())
	}
}

/// A sink that hands each datagram to a caller-supplied closure, for embedding
/// the pacer in another pipeline (write to a file, push onto a queue, forward to
/// a foreign FFI callback). The closure is synchronous: copy the bytes out, it
/// must not block for long.
pub struct CallbackSink<F> {
	callback: F,
}

impl<F> CallbackSink<F>
where
	F: FnMut(&[u8]) -> Result<()> + Send,
{
	/// Wrap a closure as a sink.
	pub fn new(callback: F) -> Self {
		Self { callback }
	}
}

impl<F> Sink for CallbackSink<F>
where
	F: FnMut(&[u8]) -> Result<()> + Send,
{
	async fn send(&mut self, datagram: &[u8]) -> Result<()> {
		(self.callback)(datagram)
	}
}

/// The RTP timestamp (90 kHz) for the moment `started_at` was `elapsed` ago.
fn rtp_timestamp_90khz(started_at: Instant) -> u32 {
	let elapsed = started_at.elapsed();
	let ticks = elapsed.as_secs().wrapping_mul(90_000) + u64::from(elapsed.subsec_nanos()) * 90_000 / 1_000_000_000;
	ticks as u32
}

/// A best-effort unique SSRC derived from the clock and process id.
fn generate_ssrc() -> u32 {
	let now = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|d| d.as_nanos() as u64)
		.unwrap_or_default();
	(now ^ u64::from(std::process::id()).rotate_left(16)) as u32
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn callback_sink_receives_datagrams() {
		let mut seen: Vec<Vec<u8>> = Vec::new();
		{
			let mut sink = CallbackSink::new(|dg: &[u8]| {
				seen.push(dg.to_vec());
				Ok(())
			});
			sink.send(&[1, 2, 3]).await.unwrap();
			sink.send(&[4, 5]).await.unwrap();
		}
		assert_eq!(seen, vec![vec![1, 2, 3], vec![4, 5]]);
	}
}
