//! The async pacing engine: a thin loop over [`Scheduler`] plus the ergonomic
//! push-style [`TsPacer`] handle.

use std::sync::Mutex;

use tokio::sync::mpsc;

use crate::config::{Bitrate, Config, DEFAULT_AUTO_FALLBACK};
use crate::error::{Error, Result};
use crate::estimate::estimate_content_bitrate;
use crate::packet::{Packet, SYNC_BYTE, TS_PACKET_SIZE};
use crate::scheduler::Scheduler;
use crate::source::Source;
use crate::stats::Stats;
use crate::{Sink, tokio_instant};

/// PCR samples to observe before locking a [`Bitrate::Auto`] rate. More samples
/// mean a longer warm-up but a steadier rate estimate.
const AUTO_WARMUP_PCRS: usize = 8;
/// Safety cap on the auto warm-up: resolve from whatever we have rather than
/// buffering unboundedly if the source has no (or very sparse) PCR.
const AUTO_WARMUP_MAX_PACKETS: usize = 20_000;

/// Run the pacer to completion: pull packets from `source`, shape them to the
/// constant bitrate in `config`, and write paced datagrams to `sink`.
///
/// This is the fully generic pipeline entry point. Returns the final [`Stats`]
/// when the source is exhausted and the buffer has drained. Any source or sink
/// (a MoQ subscriber, SRT/RIST receiver, file, socket) composes here without the
/// engine knowing what they are.
pub async fn pace<Src, Snk>(config: Config, mut source: Src, mut sink: Snk) -> Result<Stats>
where
	Src: Source,
	Snk: Sink,
{
	// Resolve an auto-rate config against a short measurement window, then run
	// the fixed-rate scheduler. Warm-up packets prime the de-jitter buffer.
	let (config, warmup) = match config.bitrate {
		Bitrate::Constant(_) => (config, Vec::new()),
		Bitrate::Auto { headroom } => {
			let (bitrate, warmup) = resolve_auto(&mut source, headroom).await?;
			(config.with_bitrate(Bitrate::Constant(bitrate)), warmup)
		}
	};

	let mut scheduler = Scheduler::new(&config);
	let mut closing = false;

	if !warmup.is_empty() {
		// Enqueue at a single instant so they fill the buffer as the initial
		// cushion rather than replaying as a startup burst.
		let now = tokio_instant::now_std();
		for packet in warmup {
			scheduler.enqueue(packet, now);
		}
	}

	loop {
		let Some(due) = scheduler.next_due() else {
			// Clock not armed yet: block for the first packet.
			match source.recv().await? {
				Some(packet) => scheduler.enqueue(packet, tokio_instant::now_std()),
				None => return Ok(scheduler.stats()),
			}
			continue;
		};

		if closing && !scheduler.has_pending() {
			return Ok(scheduler.stats());
		}

		let sleep = tokio::time::sleep_until(tokio_instant::from_std(due));
		tokio::select! {
			biased;
			// The byte clock wins: always keep the wire at the mux rate, even
			// under a backlog, so ingest can't starve emission.
			_ = sleep => {
				let datagram = scheduler.emit_datagram(tokio_instant::now_std());
				sink.send(datagram).await?;
			}
			maybe = source.recv(), if !closing => {
				match maybe? {
					Some(packet) => scheduler.enqueue(packet, tokio_instant::now_std()),
					None => closing = true,
				}
			}
		}
	}
}

/// Pull packets until enough PCRs are seen to estimate the content rate, then
/// return the locked mux rate (measured content rate plus `headroom`) and the
/// buffered warm-up packets so the caller can prime the scheduler with them.
async fn resolve_auto<Src: Source>(source: &mut Src, headroom: f64) -> Result<(u64, Vec<Packet>)> {
	let mut warmup: Vec<Packet> = Vec::new();
	let mut pcrs = 0usize;

	// Loop ends when the source is exhausted; resolve from whatever we gathered.
	while let Some(packet) = source.recv().await? {
		if packet.has_pcr() {
			pcrs += 1;
		}
		warmup.push(packet);
		if pcrs >= AUTO_WARMUP_PCRS || warmup.len() >= AUTO_WARMUP_MAX_PACKETS {
			break;
		}
	}

	let content = estimate_content_bitrate(&warmup).unwrap_or(DEFAULT_AUTO_FALLBACK);
	let bitrate = ((content as f64) * (1.0 + headroom.max(0.0))) as u64;
	Ok((bitrate.max(1), warmup))
}

/// An ergonomic, push-style handle to a running pacer.
///
/// [`spawn`](TsPacer::spawn) starts the emitter on a background task; feed it
/// with [`push_packet`](TsPacer::push_packet) / [`push_packets`](TsPacer::push_packets)
/// / [`push_bytes`](TsPacer::push_bytes) and finish with [`close`](TsPacer::close).
/// Internally this is just [`pace`] driven by an in-memory channel source, so a
/// caller who prefers the pipeline form can use [`pace`] directly instead.
pub struct TsPacer {
	tx: mpsc::Sender<Packet>,
	handle: tokio::task::JoinHandle<Result<Stats>>,
	/// Partial trailing bytes buffered across [`push_bytes`](TsPacer::push_bytes) calls.
	partial: Mutex<Vec<u8>>,
}

impl TsPacer {
	/// Start pacing to `sink` on a background task, returning a handle to push
	/// packets into.
	pub fn spawn<S>(config: Config, sink: S) -> Self
	where
		S: Sink + Send + 'static,
	{
		// Bound the channel so a stalled sink applies backpressure to the
		// producer rather than growing an unbounded queue.
		let (tx, rx) = mpsc::channel(1024);
		let handle = tokio::spawn(pace(config, ChannelSource { rx }, sink));
		Self {
			tx,
			handle,
			partial: Mutex::new(Vec::new()),
		}
	}

	/// Push a single validated packet.
	pub async fn push_packet(&self, packet: Packet) -> Result<()> {
		self.tx.send(packet).await.map_err(|_| Error::Closed)
	}

	/// Push several packets in order.
	pub async fn push_packets(&self, packets: &[Packet]) -> Result<()> {
		for packet in packets {
			self.push_packet(packet.clone()).await?;
		}
		Ok(())
	}

	/// Push a raw byte buffer of one or more (possibly partial) 188-byte packets.
	///
	/// Whole packets are forwarded; a trailing partial packet is buffered until
	/// the next call. Convenient when reading fixed-size reads off a socket or
	/// pipe that don't fall on packet boundaries.
	pub async fn push_bytes(&self, data: &[u8]) -> Result<()> {
		let packets = self.split_packets(data);
		for packet in packets {
			self.push_packet(packet).await?;
		}
		Ok(())
	}

	/// Drain whole packets out of the partial buffer under the lock, returning
	/// them so sends happen without the lock held.
	fn split_packets(&self, data: &[u8]) -> Vec<Packet> {
		let mut partial = self.partial.lock().expect("partial buffer poisoned");
		partial.extend_from_slice(data);

		let mut packets = Vec::with_capacity(partial.len() / TS_PACKET_SIZE);
		let mut offset = 0;
		while offset + TS_PACKET_SIZE <= partial.len() {
			if partial[offset] != SYNC_BYTE {
				offset += 1; // resync on the next candidate byte
				continue;
			}
			if let Ok(packet) = Packet::from_slice(&partial[offset..offset + TS_PACKET_SIZE]) {
				packets.push(packet);
			}
			offset += TS_PACKET_SIZE;
		}
		partial.drain(..offset);
		packets
	}

	/// Stop accepting input, flush the buffer, and wait for the emitter to
	/// finish, returning the final [`Stats`].
	pub async fn close(self) -> Result<Stats> {
		drop(self.tx);
		match self.handle.await {
			Ok(result) => result,
			Err(err) => Err(Error::Io(std::io::Error::other(err))),
		}
	}
}

/// A [`Source`] over the [`TsPacer`] push channel.
struct ChannelSource {
	rx: mpsc::Receiver<Packet>,
}

impl Source for ChannelSource {
	async fn recv(&mut self) -> Result<Option<Packet>> {
		Ok(self.rx.recv().await)
	}
}
