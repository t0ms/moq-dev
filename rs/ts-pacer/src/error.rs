//! Error and result types for the pacer.

/// Errors surfaced by [`crate`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
	/// A byte buffer was not a well-formed 188-byte MPEG-TS packet.
	#[error("invalid MPEG-TS packet: {0}")]
	InvalidPacket(&'static str),

	/// An output sink (or packet source) failed with an I/O error.
	#[error(transparent)]
	Io(#[from] std::io::Error),

	/// The pacer's background emitter task has stopped, so no more packets can
	/// be pushed. Inspect the task's return value (via [`crate::TsPacer::close`])
	/// for the underlying cause.
	#[error("pacer output task has stopped")]
	Closed,
}

/// Convenience alias for a [`Result`](std::result::Result) with this crate's [`Error`].
pub type Result<T> = std::result::Result<T, Error>;
