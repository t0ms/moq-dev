//! Pacer configuration.

use std::time::Duration;

/// How the pacer treats the PCR (Program Clock Reference) already present in the
/// input.
///
/// The choice depends on the downstream receiver, not on the source transport:
///
/// - [`PcrMode::Preserve`] leaves every PCR value byte-for-byte untouched and
///   only paces transmission. A soft IRD / player that recovers the clock from
///   PCR *values* and re-buffers plays this back cleanly. Note that once null
///   packets are stuffed to hit the target rate, a preserved PCR no longer sits
///   at the byte position a constant-rate demuxer would expect, so a hardware
///   IRD that checks PCR-vs-byte accuracy (`tsp -P pcrverify`) will flag it.
/// - [`PcrMode::Regenerate`] rewrites each PCR-bearing packet's PCR to the value
///   implied by its byte offset at the target rate, so `PCR == byte_offset * 8 *
///   27_000_000 / bitrate` by construction. This is what a CBR/ASI hardware IRD's
///   PCR-accuracy and repetition checks require. Only the six PCR octets (and the
///   discontinuity indicator across a genuine source discontinuity) are touched;
///   PID structure, continuity counters, and PSI/PES payloads are preserved.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PcrMode {
	/// Keep source PCR values verbatim; pace transmission only.
	Preserve,
	/// Byte-lock PCR to the output rate for hardware-IRD PCR accuracy.
	#[default]
	Regenerate,
}

/// The output bitrate target: an explicit constant rate, or one derived
/// automatically from the source.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Bitrate {
	/// Exact constant output rate, in bits per second, measured over the full
	/// 188-byte packets (content plus stuffing).
	Constant(u64),

	/// Derive the output rate from the source's measured content bitrate, times
	/// `1 + headroom`. The rate is measured over a short warm-up window (a few
	/// PCR samples) and then locked for the rest of the run, so the output is
	/// still true CBR.
	///
	/// This recovers the source's true *content* rate, not a padded original mux
	/// rate: MoQ (and most IP transports) carry only media and strip the
	/// source's null stuffing, so the original mux rate no longer exists by the
	/// time packets reach the pacer. `headroom` (e.g. `0.15` for +15%) leaves
	/// slack above the measured average for VBR peaks and the pacer's own
	/// stuffing. Too little risks buffer overflow on peaks; too much wastes
	/// bandwidth on nulls. When the source has no usable PCR to measure, the
	/// rate falls back to [`DEFAULT_AUTO_FALLBACK`].
	Auto {
		/// Fractional headroom above the measured content rate (`0.15` = +15%).
		headroom: f64,
	},
}

/// Configuration for a [`crate::TsPacer`] / [`crate::pace`] run.
///
/// Construct with [`Config::new`] (an explicit target bitrate) or
/// [`Config::auto`] (derive the rate from the source), then refine with the
/// `with_*` setters. The struct is `#[non_exhaustive]` so future options can be
/// added without a breaking change; build it through the constructors rather than
/// a struct literal.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct Config {
	/// Output bitrate target: a fixed [`Bitrate::Constant`] rate or
	/// [`Bitrate::Auto`] measured from the source. A constant rate must be at
	/// least the peak instantaneous input rate or the jitter buffer will
	/// overflow.
	pub bitrate: Bitrate,

	/// De-jitter buffer target: how long to prime the buffer before the first
	/// packet is emitted, and the steady-state cushion the scheduler aims to
	/// hold. Larger values absorb more input burst at the cost of latency.
	pub latency: Duration,

	/// Hard upper bound on buffered media. Input beyond this depth is dropped
	/// (oldest first) to keep latency and memory bounded rather than growing
	/// without limit on a sustained overrun.
	pub max_latency: Duration,

	/// How the PCR is handled. See [`PcrMode`].
	pub pcr: PcrMode,

	/// Number of 188-byte packets coalesced into each output datagram. The
	/// broadcast default is 7 (7 * 188 = 1316 bytes, the standard MPEG-TS-over-
	/// UDP/RTP payload that fits a 1500-byte MTU).
	pub packets_per_datagram: usize,

	/// Maximum PCR repetition interval to hold on the output (TR 101 290 P1 caps
	/// this at 40 ms). Under [`PcrMode::Regenerate`], when the source's PCR is
	/// sparser than this, the pacer inserts extra byte-locked PCR-only packets on
	/// the PCR PID (using otherwise-null rate-stuffing slots) so a hardware IRD's
	/// PLL never starves. Ignored under [`PcrMode::Preserve`], which inherits the
	/// source's PCR cadence untouched.
	pub pcr_max_interval: Duration,
}

/// Default packets per output datagram: 7 * 188 = 1316 bytes.
pub const DEFAULT_PACKETS_PER_DATAGRAM: usize = 7;
/// Default de-jitter priming latency.
pub const DEFAULT_LATENCY: Duration = Duration::from_millis(200);
/// Default hard latency bound.
pub const DEFAULT_MAX_LATENCY: Duration = Duration::from_millis(2_000);
/// Default maximum PCR repetition interval (TR 101 290 P1 limit).
pub const DEFAULT_PCR_MAX_INTERVAL: Duration = Duration::from_millis(40);
/// Default headroom for [`Bitrate::Auto`]: 15% above the measured content rate.
pub const DEFAULT_AUTO_HEADROOM: f64 = 0.15;
/// Fallback output rate for [`Bitrate::Auto`] when the source carries no usable
/// PCR to measure (bits per second).
pub const DEFAULT_AUTO_FALLBACK: u64 = 4_000_000;

impl Config {
	/// Create a config for the given target `bitrate` (bits per second) with
	/// default latency, PCR regeneration, and 7-packet datagrams.
	pub fn new(bitrate: u64) -> Self {
		Self::with(Bitrate::Constant(bitrate))
	}

	/// Create a config that derives the output rate from the source's measured
	/// content bitrate (see [`Bitrate::Auto`]), with [`DEFAULT_AUTO_HEADROOM`].
	///
	/// Auto-rate adds a short measurement window before the first packet is
	/// emitted, so the effective startup latency is higher than an explicit
	/// [`Config::new`]. Prefer an explicit rate when you know it and want minimal
	/// latency.
	pub fn auto() -> Self {
		Self::with(Bitrate::Auto {
			headroom: DEFAULT_AUTO_HEADROOM,
		})
	}

	fn with(bitrate: Bitrate) -> Self {
		Self {
			bitrate,
			latency: DEFAULT_LATENCY,
			max_latency: DEFAULT_MAX_LATENCY,
			pcr: PcrMode::default(),
			packets_per_datagram: DEFAULT_PACKETS_PER_DATAGRAM,
			pcr_max_interval: DEFAULT_PCR_MAX_INTERVAL,
		}
	}

	/// Set the output bitrate target.
	pub fn with_bitrate(mut self, bitrate: Bitrate) -> Self {
		self.bitrate = bitrate;
		self
	}

	/// The resolved constant output rate, or `None` for [`Bitrate::Auto`] (which
	/// is resolved from the source at run time by [`crate::pace`] /
	/// [`crate::TsPacer`]).
	pub fn resolved_bitrate(&self) -> Option<u64> {
		match self.bitrate {
			Bitrate::Constant(bitrate) => Some(bitrate),
			Bitrate::Auto { .. } => None,
		}
	}

	/// Set the de-jitter priming latency.
	pub fn with_latency(mut self, latency: Duration) -> Self {
		self.latency = latency;
		self
	}

	/// Set the hard upper bound on buffered media.
	pub fn with_max_latency(mut self, max_latency: Duration) -> Self {
		self.max_latency = max_latency;
		self
	}

	/// Set the PCR handling mode.
	pub fn with_pcr_mode(mut self, pcr: PcrMode) -> Self {
		self.pcr = pcr;
		self
	}

	/// Set the number of packets coalesced into each output datagram.
	pub fn with_packets_per_datagram(mut self, packets: usize) -> Self {
		self.packets_per_datagram = packets.max(1);
		self
	}

	/// Set the maximum PCR repetition interval (PCR re-insertion threshold).
	pub fn with_pcr_max_interval(mut self, interval: Duration) -> Self {
		self.pcr_max_interval = interval;
		self
	}
}
