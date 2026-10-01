//! Lays a TS export's packets onto its PCR grid.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use mpeg2ts::ts::TsPacket;

use super::export::PCR_INTERVAL;

/// Grid slots per second, so a multiplex rate in bits per second is also the per-slot
/// allowance in [`PACKET`] units.
const SLOTS_PER_SECOND: u64 = (Duration::from_secs(1).as_nanos() / PCR_INTERVAL.as_nanos()) as u64;
const _: () = assert!(
	Duration::from_secs(1)
		.as_nanos()
		.is_multiple_of(PCR_INTERVAL.as_nanos())
);
/// One packet in the fixed-point unit of the rate credit: one slot at `rate` bits per second
/// is exactly `rate` units, so no slot rounds on its own and the remainder carries over.
const PACKET: u64 = TsPacket::SIZE as u64 * 8 * SLOTS_PER_SECOND;
/// How many empty slots to lay at most before the next packets: one second's worth. Past
/// this the media had an outage rather than a coarse cadence, and a dense clock history for
/// a span that carried no bytes only stalls anything pacing on the asserted values.
const BACKFILL: u128 = 40;
/// A TS packet's payload, the most of it a decoder buffer can receive.
const PAYLOAD: usize = 184;

/// The grid slot a unit whose last byte has to arrive by `nanos` must be sent by: the
/// slot's bytes are timed up to its boundary.
pub(super) fn slot(nanos: u128) -> u128 {
	nanos / PCR_INTERVAL.as_nanos()
}

/// One access unit's packets, with the tables muxed ahead of it, still to go out.
struct Unit {
	/// The last slot its packets may ride: the one that ends by its decode time.
	due: u128,
	/// The first slot an unpadded stream spreads it from: the slot after the unit before it.
	start: u128,
	packets: Vec<u8>,
	/// Packets already laid out.
	sent: usize,
	keyframe: bool,
	/// The PID of the unit's last packet: its elementary stream.
	pid: u16,
	/// Experimental: the access units it carries, decoded evenly over `span` slots from `due`.
	frames: usize,
	span: u128,
	/// Pairs the unit with its [`Decoding`] entry.
	seq: u64,
	/// It was pushed after its due slot had been laid out, so no schedule could meet it.
	late: bool,
}

impl Unit {
	fn remaining(&self) -> usize {
		self.packets.len() / TsPacket::SIZE - self.sent
	}

	/// The slot its next packet is due by.
	fn next_due(&self) -> u128 {
		let count = self.packets.len() / TsPacket::SIZE;
		frame_due(self.due, self.span, self.frames, self.sent * self.frames / count.max(1))
	}

	/// The packets that must have arrived by the end of slot `index`.
	fn required(&self, index: u128) -> usize {
		let count = self.packets.len() / TsPacket::SIZE;
		let decoded = (0..self.frames)
			.take_while(|&k| frame_due(self.due, self.span, self.frames, k) <= index)
			.count();
		(decoded * count).div_ceil(self.frames)
	}
}

/// The slot frame `k` of a unit's `frames` decodes after: the last its bytes may ride.
fn frame_due(due: u128, span: u128, frames: usize, k: usize) -> u128 {
	due + span * k as u128 / frames as u128
}

/// The packets of a `count`-packet unit that carry frame `k` of its `frames`.
fn frame_packets(count: usize, frames: usize, k: usize) -> usize {
	((k + 1) * count).div_ceil(frames) - (k * count).div_ceil(frames)
}

/// A buffered unit not yet wholly decoded.
struct Decoding {
	seq: u64,
	due: u128,
	pid: u16,
	count: usize,
	frames: usize,
	span: u128,
	/// Frames already decoded.
	removed: usize,
	/// Packets sent so far.
	sent: usize,
}

impl Decoding {
	/// The packets of frames already decoded: they leave the buffer as soon as they arrive.
	fn decoded(&self) -> usize {
		(self.removed * self.count).div_ceil(self.frames)
	}
}

/// One grid slot's worth of output.
pub(super) struct Slot {
	pub index: u128,
	/// The media packets, in the order they were muxed.
	pub packets: Vec<u8>,
	/// How many null packets pad the slot to the multiplex rate, its clock packet included.
	pub nulls: usize,
	/// Whether a keyframe's first packet rides in the slot.
	pub keyframe: bool,
}

impl Slot {
	/// The slot's packets after its clock packet: the media spread evenly among the null
	/// packets, and each PID's packets spread evenly among the others, every PID keeping its
	/// own order.
	///
	/// A low-rate stream muxed whole, like an audio frame's few packets or two frames back to
	/// back, would otherwise arrive at the multiplex's full rate and overflow its 512-byte
	/// transport buffer, which drains at 2 Mb/s (ISO 13818-1 2.4.2.3). The program tables (PAT
	/// and the PMT on `pmt_pid`) move ahead of every packet muxed after them, so they still
	/// lead the keyframe they were written for and a reader knows each PID before its first
	/// packet.
	pub fn layout(&self, pmt_pid: u16, null: &[u8]) -> Vec<u8> {
		let packets: Vec<&[u8]> = self
			.packets
			.as_chunks::<{ TsPacket::SIZE }>()
			.0
			.iter()
			.map(|p| p.as_slice())
			.collect();
		let pid = |packet: &[u8]| u16::from(packet[1] & 0x1f) << 8 | u16::from(packet[2]);
		let mut counts: HashMap<u16, u64> = HashMap::new();
		for packet in &packets {
			*counts.entry(pid(packet)).or_default() += 1;
		}

		// The k-th of a PID's n packets sits at (2k + 1) / 2n of the way through.
		let mut seen: HashMap<u16, u64> = HashMap::new();
		let mut keys: Vec<(u64, u64)> = packets
			.iter()
			.map(|packet| {
				let pid = pid(packet);
				let k = seen.entry(pid).or_default();
				*k += 1;
				(2 * *k - 1, 2 * counts[&pid])
			})
			.collect();
		let cmp = |(an, ad): (u64, u64), (bn, bd): (u64, u64)| {
			(u128::from(an) * u128::from(bd)).cmp(&(u128::from(bn) * u128::from(ad)))
		};
		let mut after = (1, 1);
		for (packet, key) in packets.iter().zip(keys.iter_mut()).rev() {
			let pid = pid(packet);
			if pid == 0 || pid == pmt_pid {
				*key = after;
			} else if cmp(*key, after).is_lt() {
				after = *key;
			}
		}
		let mut order: Vec<usize> = (0..packets.len()).collect();
		order.sort_by(|&a, &b| cmp(keys[a], keys[b]));

		// Media packet i of k lands at (2i + 1) / 2k of the way through, nulls in between.
		let (k, total) = (order.len(), order.len() + self.nulls);
		let mut out = Vec::with_capacity(total * TsPacket::SIZE);
		let mut next = 0;
		for at in 0..total {
			if next < k && at == (2 * next + 1) * total / (2 * k) {
				out.extend_from_slice(packets[order[next]]);
				next += 1;
			} else {
				out.extend_from_slice(null);
			}
		}
		out
	}
}

/// Packs muxed access units onto the PCR grid.
///
/// With a multiplex rate each slot carries exactly what the rate allows, padded with null
/// packets, and each unit goes out as late as the rate lets it while still arriving by its
/// decode time. A unit bigger than one slot (a keyframe) spreads over the slots before its
/// decode time, back as far as the `window`; one that cannot fit there fails the export
/// rather than overrun the rate or arrive late. Sending no earlier than needed keeps a
/// receiver's buffers as empty as the schedule allows.
///
/// Without a rate the stream is unpadded, and each unit spreads evenly over the slots
/// between the unit before it and its own decode time.
pub(super) struct Schedule {
	units: VecDeque<Unit>,
	/// Slots before its due slot a unit may go out: the export's delay.
	window: u128,
	/// The multiplex rate, in bits per second.
	rate: Option<u64>,
	/// The rate's allowance not yet spent, in [`PACKET`] units.
	credit: u64,
	/// The next slot to lay out.
	next: Option<u128>,
	/// The due slot of the last unit pushed.
	last: Option<u128>,
	/// Slots before this one may overrun the rate ([`Self::set_rate`]).
	grace: Option<u128>,
	/// Experimental: a decoder-buffer size in bytes per PID. Units on these PIDs are also sent
	/// earliest deadline first, ahead of the as-late-as-possible floor, while the buffer has room.
	buffers: HashMap<u16, usize>,
	/// Bytes sent and not yet decoded, per buffered PID.
	occupancy: HashMap<u16, usize>,
	/// Buffered units by due slot, with their size, for removal at decode time.
	decoding: VecDeque<Decoding>,
	/// Experimental: a transport-buffer drain rate in bits per second per PID.
	drains: HashMap<u16, u64>,
	/// Experimental: the access units per unit on a PID whose units carry several.
	frames: HashMap<u16, usize>,
	/// The last span measured per PID, assumed for a unit whose successor is still to come.
	spans: HashMap<u16, u128>,
	seq: u64,
	/// Experimental: the first slot laid out. The authored DTS of an open GOP's leading pictures
	/// can bunch them past what any schedule carries, so lateness before two windows on is not fatal.
	began: Option<u128>,
	/// Experimental (`MOQ_TS_LATE=send`): a missed deadline is counted and the unit sent late,
	/// rather than failing the export.
	send_late: bool,
	missed: u64,
}

/// The transport-buffer drain rate of a PID with none given (ISO 13818-1 Rxsys).
const DEFAULT_RX: u64 = 1_000_000;

/// `MOQ_TS_EB=pid=bytes[,...]` decoder-buffer sizes, `MOQ_TS_RX=pid=bps[,...]` drain rates,
/// `MOQ_TS_FRAMES=pid=count[,...]` access units per unit.
fn from_env<T: std::str::FromStr>(name: &str) -> HashMap<u16, T> {
	std::env::var(name)
		.ok()
		.map(|spec| {
			spec.split(',')
				.filter_map(|entry| {
					let (pid, bytes) = entry.split_once('=')?;
					Some((pid.trim().parse().ok()?, bytes.trim().parse().ok()?))
				})
				.collect()
		})
		.unwrap_or_default()
}

impl Schedule {
	pub fn new(window: Duration) -> Self {
		Self {
			units: VecDeque::new(),
			window: slot(window.as_nanos()),
			rate: None,
			credit: 0,
			next: None,
			last: None,
			grace: None,
			buffers: from_env("MOQ_TS_EB"),
			drains: from_env("MOQ_TS_RX"),
			frames: from_env("MOQ_TS_FRAMES"),
			spans: HashMap::new(),
			seq: 0,
			began: None,
			send_late: std::env::var("MOQ_TS_LATE").is_ok_and(|v| v == "send"),
			missed: 0,
			occupancy: HashMap::new(),
			decoding: VecDeque::new(),
		}
	}

	/// Pad to `rate` bits per second from here on, or stop padding.
	///
	/// A rate that turns up mid-stream (import records one only once it has measured it)
	/// finds units already queued with less than a window of room before them, so for one
	/// window those may overrun the rate rather than fail the export.
	pub fn set_rate(&mut self, rate: Option<u64>) {
		if self.rate != rate {
			self.rate = rate;
			self.credit = 0;
			self.grace = self.next.map(|next| next + self.window);
		}
	}

	/// Queue an access unit (whole TS packets) whose last byte has to arrive by `by`
	/// nanoseconds: its decode time, less what the receiver needs to pass the last packet on.
	///
	/// Units go out in the order they are pushed, which keeps the program tables ahead of
	/// what they describe. The jitter buffer hands them over in decode order; a unit that
	/// decodes before one pushed ahead of it (frames read in arrival order under a zero
	/// delay) is due with it.
	pub fn push(&mut self, by: u128, packets: Vec<u8>, keyframe: bool) {
		let due = slot(by);
		// Scheduled per PID, units keep only their own PID's order, so none is pulled earlier.
		for unit in self.units.iter_mut().rev().take_while(|_| self.buffers.is_empty()) {
			if unit.due <= due {
				break;
			}
			unit.due = due;
			unit.start = unit.start.min(due);
		}
		let start = self.last.map_or(due, |last| (last + 1).min(due));
		self.last = Some(due);
		let pid = packets.len().checked_sub(TsPacket::SIZE).map_or(0, |at| {
			(u16::from(packets[at + 1] & 0x1f) << 8) | u16::from(packets[at + 2])
		});
		let frames = self.frames.get(&pid).copied().unwrap_or(1).max(1);
		let span = match frames > 1 {
			true => {
				if let Some(unit) = self.units.iter_mut().rev().find(|unit| unit.pid == pid) {
					unit.span = due.saturating_sub(unit.due);
				}
				if let Some(entry) = self.decoding.iter_mut().rev().find(|entry| entry.pid == pid) {
					entry.span = due.saturating_sub(entry.due);
					self.spans.insert(pid, entry.span);
				}
				self.spans.get(&pid).copied().unwrap_or(0)
			}
			false => 0,
		};
		self.seq += 1;
		let late = self.next.is_some_and(|next| due < next);
		tracing::debug!(pid, due, next = self.next.unwrap_or(0), late, "ts unit pushed");
		if self.buffers.contains_key(&pid) {
			self.decoding.push_back(Decoding {
				seq: self.seq,
				due,
				pid,
				count: packets.len() / TsPacket::SIZE,
				frames,
				span,
				removed: 0,
				sent: 0,
			});
		}
		self.units.push_back(Unit {
			due,
			start,
			packets,
			sent: 0,
			keyframe,
			pid,
			frames,
			span,
			seq: self.seq,
			late,
		});
	}

	pub fn is_empty(&self) -> bool {
		self.units.is_empty()
	}

	/// Drop everything queued and start the grid afresh.
	pub fn clear(&mut self) {
		self.units.clear();
		self.next = None;
		self.last = None;
		self.grace = None;
		self.credit = 0;
		self.occupancy.clear();
		self.decoding.clear();
		self.began = None;
	}

	/// Lay out the next slot, if it is settled.
	///
	/// `known` is the first slot a unit still to be pushed could be due in, which settles
	/// every slot whose window ends before it; `None` means nothing more is coming, so every
	/// queued unit goes out.
	pub fn next(&mut self, known: Option<u128>) -> anyhow::Result<Option<Slot>> {
		let first = match (&self.rate, self.units.front()) {
			(Some(_), Some(unit)) => Some(unit.due.saturating_sub(self.window)),
			(None, Some(_)) => self.units.iter().map(|unit| unit.start).min(),
			(_, None) => None,
		};
		let index = match (self.next, first) {
			(None, None) => return Ok(None),
			(None, Some(first)) => first,
			// Skip the empty slots of a long gap, keeping the second before what comes next.
			(Some(next), first) => {
				let window = self.rate.map_or(0, |_| self.window);
				let target = first.or(known.map(|known| known.saturating_sub(window)));
				next.max(target.map_or(0, |target| target.saturating_sub(BACKFILL)))
			}
		};
		// An unpadded unit spreads only back to the one before it, so it needs no window.
		let window = self.rate.map_or(0, |_| self.window);
		match known {
			Some(known) if index + window >= known => return Ok(None),
			None if self.units.is_empty() => return Ok(None),
			_ => {}
		}
		self.next = Some(index + 1);

		let (take, nulls) = match self.rate {
			Some(rate) => {
				self.credit += rate;
				let allowed = (self.credit / PACKET).max(1);
				self.credit -= (self.credit / PACKET).min(allowed) * PACKET;
				if !self.buffers.is_empty() {
					let began = *self.began.get_or_insert(index);
					let take = self.admit(index, allowed as usize - 1);
					let late = self
						.units
						.iter()
						.zip(take.iter())
						.position(|(unit, take)| !unit.late && unit.sent + *take < unit.required(index));
					let excused = self.grace.is_some_and(|grace| index < grace) || index < began + 2 * self.window;
					if let Some(i) = late.filter(|_| !excused) {
						let unit = &mut self.units[i];
						anyhow::ensure!(
							self.send_late,
							"MPEG-TS output missed a decode deadline on PID {} in a {}ms slot at {rate} b/s; raise the delay or the multiplex rate",
							unit.pid,
							PCR_INTERVAL.as_millis(),
						);
						unit.late = true;
						self.missed += 1;
						if self.missed.is_power_of_two() {
							tracing::warn!(
								pid = unit.pid,
								missed = self.missed,
								"MPEG-TS unit missed its decode deadline; sending it late"
							);
						}
					}
					let media = take.iter().sum::<usize>();
					(take, (allowed as usize - 1).saturating_sub(media))
				} else {
					let media = self.needed(index, rate / PACKET);
					let overrun = media >= allowed as usize;
					anyhow::ensure!(
						!overrun || self.grace.is_some_and(|grace| index < grace),
						"MPEG-TS output needs {media} packets in a {}ms slot, more than {rate} b/s allows within the delay; raise the delay or the multiplex rate",
						PCR_INTERVAL.as_millis()
					);
					let mut left = media;
					let take: Vec<usize> = self
						.units
						.iter()
						.map(|unit| {
							let take = unit.remaining().min(left);
							left -= take;
							take
						})
						.collect();
					(take, (allowed as usize - 1).saturating_sub(media))
				}
			}
			None => {
				let take = self
					.units
					.iter()
					.map(|unit| match unit.start <= index {
						true => unit.remaining().div_ceil((unit.due.max(index) - index + 1) as usize),
						false => 0,
					})
					.collect();
				(take, 0)
			}
		};

		let mut packets = Vec::new();
		let mut keyframe = false;
		for (unit, take) in self.units.iter_mut().zip(take) {
			if take == 0 {
				continue;
			}
			keyframe |= unit.keyframe && unit.sent == 0;
			let from = unit.sent * TsPacket::SIZE;
			packets.extend_from_slice(&unit.packets[from..from + take * TsPacket::SIZE]);
			unit.sent += take;
		}
		self.units.retain(|unit| unit.remaining() > 0);
		Ok(Some(Slot {
			index,
			packets,
			nulls,
			keyframe,
		}))
	}

	/// Experimental: each slot's packets chosen per PID, earliest deadline first, every unit
	/// sent as soon as it is released and its PID's buffers admit it.
	///
	/// A PID takes no more packets in a slot than its transport buffer drains in one (the
	/// layout spreads them evenly through the slot), and a buffered PID no more than its
	/// decoder buffer has room for. A PID with no decoder buffer given goes only in its due
	/// slot or the one before, since nothing bounds how early it may arrive.
	fn admit(&mut self, index: u128, mut budget: usize) -> Vec<usize> {
		// A unit due in slot k decodes during slot k + 1 (slots are timed up to their end
		// boundary), so its bytes leave the buffer only once that slot is past. Units of
		// different PIDs are not pushed in due order, so every entry is checked.
		for entry in self.decoding.iter_mut() {
			if entry.due + 1 >= index {
				continue;
			}
			let held = self.occupancy.entry(entry.pid).or_default();
			while entry.removed < entry.frames
				&& frame_due(entry.due, entry.span, entry.frames, entry.removed) + 1 < index
			{
				let first = entry.decoded();
				let packets = frame_packets(entry.count, entry.frames, entry.removed);
				let arrived = entry.sent.saturating_sub(first).min(packets);
				*held = held.saturating_sub(arrived * PAYLOAD);
				entry.removed += 1;
			}
		}
		self.decoding.retain(|entry| entry.removed < entry.frames);
		let slot_ns = PCR_INTERVAL.as_nanos() as u64;
		let mut drained: HashMap<u16, usize> = HashMap::new();
		let mut blocked: Vec<u16> = Vec::new();
		let mut take = vec![0; self.units.len()];
		// Earliest deadline first; the sort is stable, so each PID keeps its order.
		let mut order: Vec<usize> = (0..self.units.len()).collect();
		order.sort_by_key(|&i| self.units[i].next_due());
		for i in order {
			let (unit, take) = (&self.units[i], &mut take[i]);
			if budget == 0 {
				break;
			}
			if blocked.contains(&unit.pid) {
				continue;
			}
			let released = match self.buffers.contains_key(&unit.pid) {
				true => unit.due.saturating_sub(self.window) <= index,
				false => unit.due <= index + 1,
			};
			if !released {
				blocked.push(unit.pid);
				continue;
			}
			let rx = self.drains.get(&unit.pid).copied().unwrap_or(DEFAULT_RX);
			let cap = ((rx * slot_ns / 1_000_000_000) / (TsPacket::SIZE as u64 * 8)).saturating_sub(1) as usize;
			let sent = drained.entry(unit.pid).or_default();
			let mut extra = unit.remaining().min(budget).min(cap.saturating_sub(*sent));
			if let Some(&size) = self.buffers.get(&unit.pid) {
				let held = self.occupancy.entry(unit.pid).or_default();
				extra = extra.min(size.saturating_sub(*held) / PAYLOAD);
				if let Some(entry) = self.decoding.iter_mut().find(|entry| entry.seq == unit.seq) {
					let stays = (entry.sent + extra).saturating_sub(entry.sent.max(entry.decoded()));
					*held += stays * PAYLOAD;
					entry.sent += extra;
				}
			}
			*take = extra;
			*sent += extra;
			budget -= extra;
			if extra < unit.remaining() {
				blocked.push(unit.pid);
			}
		}
		take
	}

	/// The fewest packets slot `index` must carry so every queued unit still arrives by its
	/// due slot, with `per_slot` packets of room in each slot after it.
	fn needed(&self, index: u128, per_slot: u64) -> usize {
		let mut queued = 0;
		let mut needed = 0;
		for unit in &self.units {
			queued += unit.remaining();
			let room = (unit.due.saturating_sub(index) as usize).saturating_mul(per_slot.saturating_sub(1) as usize);
			needed = needed.max(queued.saturating_sub(room));
		}
		needed
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A unit of `count` packets on PID `pid`.
	fn unit(pid: u8, count: usize) -> Vec<u8> {
		let mut packet = [0u8; TsPacket::SIZE];
		packet[0] = 0x47;
		packet[2] = pid;
		packet[3] = 0x10;
		packet.repeat(count)
	}

	fn ms(ms: u128) -> u128 {
		ms * 1_000_000
	}

	/// 40 packets per slot, the clock packet included.
	const RATE: u64 = 40 * PACKET;

	#[test]
	fn a_unit_goes_out_in_its_due_slot() {
		let mut schedule = Schedule::new(Duration::from_millis(100));
		schedule.set_rate(Some(RATE));
		schedule.push(ms(1_000), unit(1, 3), true);
		// Nothing past the window is settled until a later unit shows up.
		let mut slots = Vec::new();
		while let Some(slot) = schedule.next(Some(slot(ms(1_200)))).unwrap() {
			slots.push((
				slot.index,
				slot.packets.len() / TsPacket::SIZE,
				slot.nulls,
				slot.keyframe,
			));
		}
		assert_eq!(
			slots.first().map(|s| s.0),
			Some(36),
			"starts a window ahead of the unit"
		);
		assert!(
			slots.iter().all(|s| s.1 + s.2 == 39),
			"every slot is padded to the rate"
		);
		let sent: Vec<_> = slots.iter().filter(|s| s.1 > 0).collect();
		assert_eq!(sent, [&(40, 3, 36, true)], "as late as it can go: {slots:?}");
	}

	#[test]
	fn a_burst_spreads_over_the_slots_before_it() {
		let mut schedule = Schedule::new(Duration::from_millis(100));
		schedule.set_rate(Some(RATE));
		schedule.push(ms(1_000), unit(1, 100), true);
		let mut sent = Vec::new();
		while let Some(slot) = schedule.next(None).unwrap() {
			sent.push((slot.index, slot.packets.len() / TsPacket::SIZE));
		}
		// 39 media packets fit each slot, so 100 packets take the last three before its due.
		assert_eq!(sent, [(36, 0), (37, 0), (38, 22), (39, 39), (40, 39)]);
	}

	#[test]
	fn a_burst_too_big_for_the_window_fails() {
		let mut schedule = Schedule::new(Duration::from_millis(50));
		schedule.set_rate(Some(RATE));
		schedule.push(ms(1_000), unit(1, 200), true);
		let err = std::iter::from_fn(|| schedule.next(None).transpose()).find_map(Result::err);
		assert!(err.is_some(), "a burst past the window must fail the export");
	}

	/// A rate that turns up mid-stream lets the units already queued overrun it for one
	/// window, rather than fail the export.
	#[test]
	fn a_late_rate_overruns_for_one_window() {
		let mut schedule = Schedule::new(Duration::from_millis(100));
		schedule.push(ms(1_000), unit(1, 4), true);
		while schedule.next(Some(slot(ms(1_050)))).unwrap().is_some() {}
		schedule.set_rate(Some(RATE));
		schedule.push(ms(1_100), unit(1, 150), true);
		let mut sent = Vec::new();
		while let Some(slot) = schedule.next(None).unwrap() {
			sent.push((slot.index, slot.packets.len() / TsPacket::SIZE, slot.nulls));
		}
		assert_eq!(sent, [(42, 72, 0), (43, 39, 0), (44, 39, 0)]);

		schedule.push(ms(3_000), unit(1, 2_000), true);
		let err = std::iter::from_fn(|| schedule.next(None).transpose()).find_map(Result::err);
		assert!(err.is_some(), "past the window, a burst that does not fit fails");
	}

	#[test]
	fn unpadded_units_spread_up_to_their_decode_time() {
		let mut schedule = Schedule::new(Duration::ZERO);
		schedule.push(ms(1_000), unit(1, 4), true);
		schedule.push(ms(1_100), unit(1, 4), false);
		let mut sent = Vec::new();
		while let Some(slot) = schedule.next(None).unwrap() {
			sent.push((slot.index, slot.packets.len() / TsPacket::SIZE, slot.nulls));
		}
		assert_eq!(sent, [(40, 4, 0), (41, 1, 0), (42, 1, 0), (43, 1, 0), (44, 1, 0)]);
	}
}
