//! Thin MoQ egress adapter: pace a subscriber's MPEG-TS output to stdout, UDP,
//! or RTP.
//!
//! The MoQ subscriber is just a producer of TS packets, so the whole integration
//! is a pipe:
//!
//! ```text
//! # Pipe the paced stream straight on, like the subscriber itself:
//! moq ... export ts | cargo run -p ts-pacer --example moq_egress -- - auto | ffplay -i -
//!
//! # Or push it to a UDP/RTP multicast group:
//! moq ... export ts | cargo run -p ts-pacer --example moq_egress -- 239.0.0.1:5000 10000000 --rtp
//! ```
//!
//! Nothing here references MoQ: `export ts` writes transport packets to stdout,
//! [`ReadSource`] reads them, and the pacer shapes them to a broadcast-compliant
//! CBR stream. Swap the first stage for SRT, RIST, or a file and the rest is
//! unchanged.

use std::net::SocketAddr;
use std::time::Duration;

use ts_pacer::{Config, PcrMode, ReadSource, RtpSink, Stats, UdpSink, WriteSink, pace};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
	let mut args = std::env::args().skip(1);
	let usage = "usage: moq_egress <-|stdout|dest_ip:port> <bitrate_bps|auto> [--rtp] [--preserve] [--latency-ms N]";
	let dest = args.next().expect(usage);
	let rate = args.next().expect(usage);

	let mut rtp = false;
	let mut pcr = PcrMode::Regenerate;
	let mut latency = Duration::from_millis(200);
	let mut rest = args.peekable();
	while let Some(arg) = rest.next() {
		match arg.as_str() {
			"--rtp" => rtp = true,
			"--preserve" => pcr = PcrMode::Preserve,
			"--latency-ms" => {
				let ms: u64 = rest.next().expect("--latency-ms needs a value").parse()?;
				latency = Duration::from_millis(ms);
			}
			other => panic!("unknown argument {other:?}; {usage}"),
		}
	}

	let source = ReadSource::new(tokio::io::stdin());
	let config = if rate == "auto" {
		Config::auto()
	} else {
		Config::new(rate.parse().expect("bitrate must be an integer or \"auto\""))
	}
	.with_latency(latency)
	.with_pcr_mode(pcr);

	let pcr_desc = if pcr == PcrMode::Regenerate {
		"regenerate PCR"
	} else {
		"preserve PCR"
	};

	let stats = if dest == "-" || dest == "stdout" {
		eprintln!(
			"ts-pacer: -> stdout (raw ts) @ {rate} b/s, {pcr_desc}, {} ms latency",
			latency.as_millis()
		);
		pace(config, source, WriteSink::new(tokio::io::stdout())).await?
	} else {
		let destination: SocketAddr = dest.parse().expect("dest must be ip:port, - or stdout");
		let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
		eprintln!(
			"ts-pacer: -> {}{destination} @ {rate} b/s, {pcr_desc}, {} ms latency",
			if rtp { "rtp://" } else { "udp://" },
			latency.as_millis(),
		);
		if rtp {
			pace(config, source, RtpSink::new(socket, destination)).await?
		} else {
			pace(config, source, UdpSink::new(socket, destination)).await?
		}
	};

	report(&stats);
	Ok(())
}

fn report(stats: &Stats) {
	eprintln!(
		"ts-pacer: done. output_packets={} content={} null={} ({:.1}% stuffing) \
		 pcr_inserted={} stripped_nulls={} dropped={} underruns={}",
		stats.output_packets,
		stats.content_packets,
		stats.null_packets,
		stats.null_ratio() * 100.0,
		stats.pcr_inserted,
		stats.input_nulls_stripped,
		stats.dropped_packets,
		stats.underruns,
	);
}
