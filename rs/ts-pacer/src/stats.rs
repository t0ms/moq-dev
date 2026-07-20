//! Pacing statistics.

/// A running tally of what the pacer has emitted and dropped.
///
/// All counters are cumulative for the life of the pacer. `null_packets /
/// output_packets` is the stuffing ratio; a climbing `dropped_packets` means the
/// input sustained a rate above the configured bitrate (or burst past
/// `max_latency`), and a climbing `underruns` means the input starved the buffer
/// so null packets were sent to hold the rate.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Stats {
	/// Total 188-byte packets written to the sink (content + null).
	pub output_packets: u64,
	/// Content (non-null) packets written to the sink.
	pub content_packets: u64,
	/// Null (stuffing) packets inserted to hold the constant bitrate.
	pub null_packets: u64,
	/// Input packets dropped because the jitter buffer was at `max_latency`.
	pub dropped_packets: u64,
	/// Input null/stuffing packets stripped at ingest (their padding is replaced
	/// by the pacer's own rate stuffing).
	pub input_nulls_stripped: u64,
	/// Output slots that emitted a null because the buffer was empty (starved).
	pub underruns: u64,
	/// PCR anchor re-bases from a genuine source discontinuity (>5 s PCR jump).
	pub pcr_rebases: u64,
	/// Byte-locked PCR-only packets inserted to hold the repetition limit.
	pub pcr_inserted: u64,
}

impl Stats {
	/// Fraction of output packets that were null stuffing, in `0.0..=1.0`.
	pub fn null_ratio(&self) -> f64 {
		if self.output_packets == 0 {
			0.0
		} else {
			self.null_packets as f64 / self.output_packets as f64
		}
	}
}
