//! A jitter buffer that releases several tracks' frames in one decode order, each a fixed
//! delay after its decode time.

use std::collections::{BTreeMap, VecDeque};
use std::pin::Pin;
use std::task::Poll;
use std::time::Duration;

use moq_net::Timestamp;
use web_async::time::{Instant, Sleep};

/// A frame handed to [`Buffer::push`].
pub(crate) struct Arrival<T> {
	/// When the frame was read from its source.
	pub arrived: Instant,
	/// When the frame decodes, nondecreasing within a track between discontinuities.
	pub decode: Timestamp,
	/// The source's discontinuity counter when the frame was read.
	pub discontinuity: u64,
	/// Whether the frame decodes without the track's earlier frames.
	pub sync: bool,
	pub item: T,
}

/// A frame [`Buffer::poll_next`] let go.
pub(crate) struct Ready<K, T> {
	pub track: K,
	/// Counts up each time a track's discontinuity started a new anchor. Tracks that
	/// cross the same discontinuity share one.
	pub generation: u64,
	pub item: T,
}

/// What [`Buffer::push`] did with a frame.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Push {
	Queued,
	/// It missed its deadline and was dropped; the track resumes at its next sync frame.
	Late,
	/// It was dropped because the track is waiting for a sync frame.
	Waiting,
}

/// Holds each track's frames until a fixed delay past their decode time, like an SRT
/// receiver's TSBPD, in `(deadline, track)` order across tracks.
///
/// The clock is anchored at the first frame's arrival. Within one anchor the deadline
/// order is the decode order, so two buffers that saw the same frames arrive with
/// different skew, each within its deadline, emit them in the same order. A frame that
/// arrives past its deadline would break that order, so it is dropped and counted.
///
/// A track whose source reports a discontinuity may have restarted its timeline, so it
/// moves to a new generation with its own anchor: the newest one when another track got
/// there first and the frame lands on its clock, otherwise one anchored at this frame,
/// though never ahead of a deadline already given out. A backlog read in one go still
/// releases each generation after the one before it, and a live stream keeps its delay.
///
/// A zero delay holds nothing and drops nothing: each frame goes out as soon as it is
/// read, ordered only among the frames read together.
pub(crate) struct Buffer<K, T> {
	delay: Duration,
	/// Each generation's anchor: the first frame's arrival and decode time.
	anchors: BTreeMap<u64, (Instant, Timestamp)>,
	tracks: BTreeMap<K, Track<T>>,
	/// The latest deadline given out.
	horizon: Option<Instant>,
	timer: Option<Pin<Box<Sleep>>>,
	dropped: u64,
}

struct Track<T> {
	/// Frames in arrival order, each with its deadline and generation.
	queue: VecDeque<(Instant, u64, T)>,
	generation: u64,
	discontinuity: u64,
	/// A frame was dropped, so later ones are too until the next sync frame.
	waiting: bool,
}

impl<K: Ord + Clone, T> Buffer<K, T> {
	pub fn new(delay: Duration) -> Self {
		Self {
			delay,
			anchors: BTreeMap::new(),
			tracks: BTreeMap::new(),
			horizon: None,
			timer: None,
			dropped: 0,
		}
	}

	/// Queue a frame, or drop it if it cannot make its deadline.
	pub fn push(&mut self, key: K, arrival: Arrival<T>) -> Push {
		// A new track counts from the start, as if it had always been there.
		let (mut generation, discontinuity) = self
			.tracks
			.get(&key)
			.map_or((0, 0), |track| (track.generation, track.discontinuity));
		if discontinuity != arrival.discontinuity {
			generation = self.rejoin(generation, &arrival);
		} else if std::env::var_os("MOQ_JITTER_FOLLOW").is_some()
			&& let Some((&latest, &anchor)) = self.anchors.last_key_value()
			&& latest > generation
			&& self
				.deadline(anchor, arrival.decode)
				.is_some_and(|deadline| arrival.arrived <= deadline)
		{
			// Another track opened a newer clock: follow it at the first frame not late on it, so the
			// output is never fed from two clocks at once. A sparse track (sections) may decode far
			// ahead of its arrival, so it is held to that clock rather than required to land on it.
			generation = latest;
		}
		let anchor = *self.anchors.entry(generation).or_insert_with(|| {
			let after = self.horizon.and_then(|horizon| horizon.checked_sub(self.delay));
			(
				after.map_or(arrival.arrived, |after| after.max(arrival.arrived)),
				arrival.decode,
			)
		});
		let deadline = self.deadline(anchor, arrival.decode);

		let track = self.tracks.entry(key).or_insert_with(|| Track {
			queue: VecDeque::new(),
			generation,
			discontinuity: 0,
			waiting: false,
		});
		track.generation = generation;
		track.discontinuity = arrival.discontinuity;
		let push = match deadline {
			_ if track.waiting && !arrival.sync => Push::Waiting,
			Some(deadline) if arrival.arrived <= deadline || self.delay.is_zero() => {
				tracing::debug!(
					decode_ms = arrival.decode.as_nanos() / 1_000_000,
					slack_ms = (deadline - arrival.arrived).as_millis() as u64,
					generation,
					"jitter queued"
				);
				track.waiting = false;
				track.queue.push_back((deadline, generation, arrival.item));
				self.horizon = self.horizon.max(Some(deadline));
				Push::Queued
			}
			// Past the deadline, or so far off the clock that no instant holds it.
			_ => {
				track.waiting = true;
				Push::Late
			}
		};
		if push != Push::Queued {
			self.dropped += 1;
		}

		// Generation 0 stays, since a track that has yet to show up starts there.
		let oldest = self.tracks.values().map(|track| track.generation).min();
		self.anchors
			.retain(|generation, _| *generation == 0 || oldest.is_none_or(|oldest| *generation >= oldest));
		push
	}

	/// The generation a track at `generation` moves to on a discontinuity: the newest, if
	/// another track opened it and this frame lands on its clock, or else a new one.
	///
	/// Landing on the clock means the frame is neither late nor held past both an on-time
	/// frame and everything already queued, which a different timeline would be.
	fn rejoin(&self, generation: u64, arrival: &Arrival<T>) -> u64 {
		let Some((&latest, &anchor)) = self.anchors.last_key_value() else {
			return generation + 1;
		};
		let lands = self.lands(anchor, arrival);
		// A frame that lands on the latest clock continues it, whether another track opened that
		// generation or this one did: a skipped group is a gap in the timeline, not a new one.
		let rejoin = std::env::var_os("MOQ_JITTER_REJOIN").is_some();
		match (latest > generation || rejoin) && lands {
			true => latest,
			false => latest.max(generation) + 1,
		}
	}

	/// Whether a frame lands on `anchor`'s clock: neither late nor held past both an on-time frame
	/// and everything already queued.
	fn lands(&self, anchor: (Instant, Timestamp), arrival: &Arrival<T>) -> bool {
		self.deadline(anchor, arrival.decode).is_some_and(|deadline| {
			let bound = (arrival.arrived + self.delay).max(self.horizon.unwrap_or(arrival.arrived));
			arrival.arrived <= deadline && deadline <= bound
		})
	}

	/// When a frame decoding at `decode` goes out under `anchor`, if any instant holds it.
	fn deadline(&self, (anchor, base): (Instant, Timestamp), decode: Timestamp) -> Option<Instant> {
		// Signed, since a frame can decode before the one that anchored its generation.
		let offset = decode.as_nanos() as i128 - base.as_nanos() as i128 + self.delay.as_nanos() as i128;
		let nanos = u64::try_from(offset.unsigned_abs()).ok()?;
		match offset >= 0 {
			true => anchor.checked_add(Duration::from_nanos(nanos)),
			false => anchor.checked_sub(Duration::from_nanos(nanos)),
		}
	}

	/// The next frame whose deadline has come, earliest deadline first, ties by track.
	pub fn poll_next(&mut self, waiter: &kio::Waiter) -> Poll<Ready<K, T>> {
		let Some((deadline, key)) = self
			.tracks
			.iter()
			.filter_map(|(key, track)| track.queue.front().map(|(deadline, ..)| (*deadline, key)))
			.min()
		else {
			self.timer = None;
			return Poll::Pending;
		};
		if !self.delay.is_zero() && Instant::now() < deadline {
			let timer = self
				.timer
				.get_or_insert_with(|| Box::pin(web_async::time::sleep_until(deadline)));
			if timer.deadline() != deadline {
				timer.as_mut().reset(deadline);
			}
			if waiter.poll_future(timer.as_mut()).is_pending() {
				return Poll::Pending;
			}
		}
		let track = key.clone();
		let (_, generation, item) = self.tracks.get_mut(&track).and_then(|t| t.queue.pop_front()).unwrap();
		Poll::Ready(Ready {
			track,
			generation,
			item,
		})
	}

	/// When the next queued frame is due.
	#[cfg(test)]
	pub fn next_deadline(&self) -> Option<Instant> {
		self.tracks
			.values()
			.filter_map(|track| track.queue.front().map(|(deadline, ..)| *deadline))
			.min()
	}

	/// Whether no frame is waiting.
	pub fn is_empty(&self) -> bool {
		self.tracks.values().all(|track| track.queue.is_empty())
	}

	/// How many frames were dropped, late or waiting for a sync frame.
	pub fn dropped(&self) -> u64 {
		self.dropped
	}

	/// Drop every queued frame and anchor, so the next frame starts the clock afresh.
	pub fn clear(&mut self) {
		self.anchors.clear();
		self.tracks.clear();
		self.horizon = None;
		self.timer = None;
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const DELAY: Duration = Duration::from_millis(100);

	fn ms(ms: u64) -> Timestamp {
		Timestamp::from_millis(ms).unwrap()
	}

	fn arrival(arrived: Instant, decode: u64, item: &'static str) -> Arrival<&'static str> {
		Arrival {
			arrived,
			decode: ms(decode),
			discontinuity: 0,
			sync: true,
			item,
		}
	}

	/// Every frame due by now, in order.
	fn due(buffer: &mut Buffer<u16, &'static str>) -> Vec<&'static str> {
		let waiter = kio::Waiter::noop();
		let mut out = Vec::new();
		while let Poll::Ready(ready) = buffer.poll_next(&waiter) {
			out.push(ready.item);
		}
		out
	}

	#[tokio::test(start_paused = true)]
	async fn releases_at_the_delay_in_decode_order() {
		let start = Instant::now();
		let mut buffer = Buffer::new(DELAY);
		assert_eq!(buffer.push(1, arrival(start, 0, "v0")), Push::Queued);
		assert_eq!(buffer.push(1, arrival(start, 40, "v40")), Push::Queued);
		// Audio arrives later than the video, with an earlier decode time.
		assert_eq!(
			buffer.push(2, arrival(start + Duration::from_millis(30), 20, "a20")),
			Push::Queued
		);
		assert_eq!(
			buffer.push(2, arrival(start + Duration::from_millis(30), 40, "a40")),
			Push::Queued
		);

		assert!(due(&mut buffer).is_empty(), "nothing is due before the delay");
		tokio::time::advance(DELAY).await;
		assert_eq!(due(&mut buffer), ["v0"]);
		tokio::time::advance(Duration::from_millis(20)).await;
		assert_eq!(due(&mut buffer), ["a20"]);
		tokio::time::advance(Duration::from_millis(20)).await;
		assert_eq!(due(&mut buffer), ["v40", "a40"], "a tie goes to the lower track");
		assert!(buffer.is_empty());
	}

	/// Two buffers fed the same frames with different arrival skew, every frame inside
	/// its deadline, emit the same order.
	#[tokio::test(start_paused = true)]
	async fn arrival_skew_does_not_change_the_order() {
		let start = Instant::now();
		let (mut early, mut late) = (Buffer::new(DELAY), Buffer::new(DELAY));
		let frames = [
			(1, 0, "v0"),
			(1, 40, "v40"),
			(1, 80, "v80"),
			(2, 0, "a0"),
			(2, 20, "a20"),
			(2, 60, "a60"),
		];
		for (key, decode, item) in frames {
			// One sees audio lag by 90ms, the other sees every frame on time.
			let skew = if key == 2 { 90 } else { 0 };
			assert_eq!(
				early.push(key, arrival(start + Duration::from_millis(decode), decode, item)),
				Push::Queued
			);
			let arrived = start + Duration::from_millis(decode + skew);
			assert_eq!(late.push(key, arrival(arrived, decode, item)), Push::Queued);
		}
		tokio::time::advance(Duration::from_secs(1)).await;
		let order = due(&mut early);
		assert_eq!(order, ["v0", "a0", "a20", "v40", "a60", "v80"]);
		assert_eq!(due(&mut late), order);
	}

	/// A late frame is dropped and counted, and the rest keep their order. A track that
	/// dropped a frame waits for its next sync frame.
	#[tokio::test(start_paused = true)]
	async fn late_frames_are_dropped_until_a_sync_frame() {
		let start = Instant::now();
		let mut buffer = Buffer::new(DELAY);
		buffer.push(1, arrival(start, 0, "v0"));
		buffer.push(2, arrival(start, 0, "a0"));
		let late = start + Duration::from_millis(200);
		let video = |decode, sync, item| Arrival {
			sync,
			..arrival(late, decode, item)
		};
		assert_eq!(buffer.push(1, video(40, false, "v40")), Push::Late);
		assert_eq!(buffer.push(1, video(200, false, "v200")), Push::Waiting);
		assert_eq!(buffer.push(1, video(240, true, "v240")), Push::Queued);
		assert_eq!(buffer.push(2, arrival(late, 180, "a180")), Push::Queued);
		assert_eq!(buffer.dropped(), 2);

		tokio::time::advance(Duration::from_secs(1)).await;
		assert_eq!(due(&mut buffer), ["v0", "a0", "a180", "v240"]);
	}

	/// A discontinuity re-anchors the track, and a track reaching the same discontinuity
	/// later joins that anchor rather than making its own.
	#[tokio::test(start_paused = true)]
	async fn a_discontinuity_re_anchors() {
		let start = Instant::now();
		let mut buffer = Buffer::new(DELAY);
		buffer.push(1, arrival(start, 5_000, "v5000"));
		buffer.push(2, arrival(start, 5_000, "a5000"));

		// The publisher restarts its timeline at zero, a second in.
		let restart = start + Duration::from_secs(1);
		let reset = |arrived, decode, item| Arrival {
			discontinuity: 1,
			..arrival(arrived, decode, item)
		};
		assert_eq!(buffer.push(1, reset(restart, 0, "v0")), Push::Queued);
		// Audio still carries its old timeline for a moment, then restarts too.
		assert_eq!(buffer.push(2, arrival(restart, 6_000, "a6000")), Push::Queued);
		let later = restart + Duration::from_millis(10);
		assert_eq!(buffer.push(2, reset(later, 0, "a0")), Push::Queued);

		tokio::time::advance(Duration::from_secs(1) + DELAY).await;
		assert_eq!(due(&mut buffer), ["v5000", "a5000", "v0", "a6000", "a0"]);
	}

	/// A backlog read in one go releases a new generation after the one before it.
	#[tokio::test(start_paused = true)]
	async fn a_backlog_releases_generations_in_turn() {
		let start = Instant::now();
		let mut buffer = Buffer::new(DELAY);
		buffer.push(1, arrival(start, 5_000, "v5000"));
		buffer.push(1, arrival(start, 5_040, "v5040"));
		let reset = Arrival {
			discontinuity: 1,
			..arrival(start, 0, "v0")
		};
		buffer.push(1, reset);
		buffer.push(2, arrival(start, 5_020, "a5020"));

		tokio::time::advance(DELAY).await;
		assert_eq!(due(&mut buffer), ["v5000"]);
		tokio::time::advance(Duration::from_millis(40)).await;
		assert_eq!(due(&mut buffer), ["a5020", "v5040", "v0"]);
	}

	/// Zero holds nothing: frames read together go out at once in decode order, and a
	/// frame behind the clock still goes out.
	#[tokio::test(start_paused = true)]
	async fn zero_delay_releases_on_arrival() {
		let start = Instant::now();
		let mut buffer = Buffer::new(Duration::ZERO);
		buffer.push(1, arrival(start, 0, "v0"));
		buffer.push(1, arrival(start, 40, "v40"));
		buffer.push(2, arrival(start, 20, "a20"));
		assert_eq!(due(&mut buffer), ["v0", "a20", "v40"]);

		let late = start + Duration::from_secs(1);
		assert_eq!(buffer.push(2, arrival(late, 40, "a40")), Push::Queued);
		assert_eq!(due(&mut buffer), ["a40"]);
		assert_eq!(buffer.dropped(), 0);
	}

	#[tokio::test(start_paused = true)]
	async fn clear_starts_a_fresh_clock() {
		let start = Instant::now();
		let mut buffer = Buffer::new(DELAY);
		buffer.push(1, arrival(start, 5_000, "old"));
		buffer.clear();
		assert!(buffer.is_empty());

		tokio::time::advance(Duration::from_secs(1)).await;
		let now = Instant::now();
		assert_eq!(buffer.push(1, arrival(now, 0, "new")), Push::Queued);
		tokio::time::advance(DELAY).await;
		assert_eq!(due(&mut buffer), ["new"]);
	}
}
