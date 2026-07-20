//! Transport-agnostic MPEG-TS constant-bitrate pacer for broadcast (IRD) egress.
//!
//! Modern IP transports (MoQ, SRT, RIST, RTP, file playback) deliver MPEG-TS in
//! bursts. Professional Integrated Receiver/Decoders (IRDs) expect a smooth,
//! constant-bitrate transport with byte-accurate PCR. This crate is the missing
//! adaptation layer between the two: feed it already-multiplexed transport
//! packets and it emits a deterministic CBR stream a hardware IRD will accept.
//!
//! It is **not** a muxer. It never demultiplexes, remultiplexes, rewrites PSI,
//! regenerates PAT/PMT, or touches continuity counters. PID structure and
//! PES/PSI payloads pass through untouched. It shapes *transmission timing* and,
//! optionally, byte-locks the PCR. The only bytes it ever rewrites are the six
//! PCR octets under [`PcrMode::Regenerate`].
//!
//! # Pipeline
//!
//! The engine is a [`Source`] -> pacer -> [`Sink`] pipeline, so any packet
//! producer is just one `Source` and any output is just one `Sink`. Nothing here
//! knows about MoQ, QUIC, objects, groups, tracks, or subscribers.
//!
//! ```no_run
//! use std::net::SocketAddr;
//! use ts_pacer::{Config, ReadSource, UdpSink, pace};
//!
//! # async fn run() -> ts_pacer::Result<()> {
//! // A MoQ subscriber writing `export ts` to a pipe is just an AsyncRead source.
//! let source = ReadSource::new(tokio::io::stdin());
//! let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
//! let sink = UdpSink::new(socket, "239.0.0.1:5000".parse::<SocketAddr>().unwrap());
//! let stats = pace(Config::new(10_000_000), source, sink).await?;
//! eprintln!("null ratio {:.1}%", stats.null_ratio() * 100.0);
//! # Ok(())
//! # }
//! ```
//!
//! # Push API
//!
//! Prefer to push packets yourself? [`TsPacer`] wraps the same engine behind a
//! background task:
//!
//! ```no_run
//! use ts_pacer::{Config, Packet, TsPacer, CallbackSink};
//!
//! # async fn run(packet: Packet) -> ts_pacer::Result<()> {
//! let pacer = TsPacer::spawn(Config::new(10_000_000), CallbackSink::new(|_dg: &[u8]| Ok(())));
//! pacer.push_packet(packet).await?;
//! let _stats = pacer.close().await?;
//! # Ok(())
//! # }
//! ```

mod config;
mod error;
mod estimate;
mod jitter_buffer;
mod null_insertion;
mod output;
mod pacer;
mod packet;
mod pcr;
mod scheduler;
mod source;
mod stats;

pub use config::{
	Bitrate, Config, DEFAULT_AUTO_FALLBACK, DEFAULT_AUTO_HEADROOM, DEFAULT_LATENCY, DEFAULT_MAX_LATENCY,
	DEFAULT_PACKETS_PER_DATAGRAM, PcrMode,
};
pub use error::{Error, Result};
pub use estimate::estimate_content_bitrate;
pub use null_insertion::NULL_PID;
pub use output::{CallbackSink, RTP_PAYLOAD_TYPE_MP2T, RtpSink, Sink, UdpSink, WriteSink};
pub use pacer::{TsPacer, pace};
pub use packet::{Packet, SYNC_BYTE, TS_PACKET_SIZE};
pub use scheduler::Scheduler;
pub use source::{IterSource, ReadSource, Source};
pub use stats::Stats;

/// Bridges between `tokio::time::Instant` (used for scheduling, so the pacer
/// honours `tokio::time::pause()` in tests) and the `std::time::Instant` the
/// synchronous [`Scheduler`] reasons about.
pub(crate) mod tokio_instant {
	use std::time::Instant;

	/// The current instant, as a `std::time::Instant` derived from tokio's clock.
	pub fn now_std() -> Instant {
		tokio::time::Instant::now().into_std()
	}

	/// Lift a `std::time::Instant` back onto tokio's clock for `sleep_until`.
	pub fn from_std(instant: Instant) -> tokio::time::Instant {
		tokio::time::Instant::from_std(instant)
	}
}
