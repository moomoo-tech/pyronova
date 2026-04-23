//! In-process-style load generator. Unlike wrk (a separate heavy
//! client process that competes with the server for CPU), this is a
//! tight tokio loop that does the minimum necessary work per request:
//! batched pipelined writes + fixed-size response counting. Used to
//! find the server's true single-machine ceiling by keeping the
//! client footprint as small as possible.
//!
//! Assumes the server returns the SAME byte-for-byte response for every
//! request (probe the first one, then count by bytes).
//!
//! Usage:
//!   load-gen <addr> <duration_secs> <connections>
//!   e.g. load-gen 127.0.0.1:8000 10 64
//!
//! Tuning notes
//! - BATCH controls pipeline depth per connection. Larger = fewer
//!   syscalls = higher throughput, but also longer tail latency.
//! - Set connections roughly to match server worker count. Going
//!   higher rarely helps on a single machine.
//! - The loader always uses tokio multi-thread runtime; it does NOT
//!   pin or QoS-elevate, so the server (if pinned) keeps its P-core
//!   priority and the loader runs on whatever's left.

use std::env;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const REQ: &[u8] = b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: keep-alive\r\n\r\n";
const BATCH: usize = 32;

async fn probe_response_size(addr: &str) -> std::io::Result<usize> {
    let mut s = TcpStream::connect(addr).await?;
    s.set_nodelay(true)?;
    s.write_all(REQ).await?;
    // Read until we have a full response. Hyper sends it in one write
    // for plaintext, but be defensive and accumulate until we've seen
    // Content-Length + header terminator + body.
    let mut buf = vec![0u8; 4096];
    let mut filled = 0usize;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if Instant::now() > deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "probe timed out",
            ));
        }
        let n = s.read(&mut buf[filled..]).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "probe EOF",
            ));
        }
        filled += n;
        let data = &buf[..filled];
        // Find header terminator.
        let Some(hdr_end) = find_subseq(data, b"\r\n\r\n") else {
            if filled == buf.len() {
                buf.resize(buf.len() * 2, 0);
            }
            continue;
        };
        // Parse Content-Length (case-insensitive).
        let headers = &data[..hdr_end];
        let mut content_len: Option<usize> = None;
        for line in headers.split(|&b| b == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.len() < 16 {
                continue;
            }
            let (k, v) = match line.iter().position(|&b| b == b':') {
                Some(i) => (&line[..i], &line[i + 1..]),
                None => continue,
            };
            if k.eq_ignore_ascii_case(b"content-length") {
                let v = std::str::from_utf8(v)
                    .ok()
                    .map(|s| s.trim())
                    .and_then(|s| s.parse::<usize>().ok());
                if let Some(n) = v {
                    content_len = Some(n);
                    break;
                }
            }
        }
        let body_len = content_len.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "probe: no Content-Length",
            )
        })?;
        let total = hdr_end + 4 + body_len;
        if filled >= total {
            return Ok(total);
        }
        // Keep reading until we have the full body.
        if buf.len() < total {
            buf.resize(total + 256, 0);
        }
    }
}

fn find_subseq(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

async fn connection_loop(
    addr: String,
    resp_size: usize,
    counter: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<()> {
    let mut s = TcpStream::connect(&addr).await?;
    s.set_nodelay(true)?;

    let mut batch_req = Vec::with_capacity(REQ.len() * BATCH);
    for _ in 0..BATCH {
        batch_req.extend_from_slice(REQ);
    }
    let need = resp_size * BATCH;
    let mut buf = vec![0u8; need.max(8192)];

    while !stop.load(Ordering::Relaxed) {
        // Pipeline: write a batch of requests, then read exactly enough
        // bytes to cover BATCH responses.
        s.write_all(&batch_req).await?;

        let mut got = 0usize;
        while got < need {
            let n = s.read(&mut buf[..]).await?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "server hung up",
                ));
            }
            got += n;
            // `buf` is reused — we only care about byte count, not
            // actual bytes. If we overran `need`, the next iteration's
            // read will start from 0 again because we don't try to
            // carry over bytes. Fine as long as responses are uniform:
            // extra bytes belong to the next batch's boundary, which
            // we'll account for by treating `got` vs `need`. Simpler
            // approach: require BATCH requests = BATCH responses and
            // trust server doesn't interleave.
        }
        counter.fetch_add(BATCH as u64, Ordering::Relaxed);
    }
    Ok(())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> std::io::Result<()> {
    let args: Vec<String> = env::args().collect();
    let addr = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "127.0.0.1:8000".to_string());
    let duration_s: u64 = args
        .get(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    let connections: usize = args
        .get(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(64);

    println!(
        "load-gen: target={addr} duration={duration_s}s connections={connections} batch={BATCH}"
    );

    // Probe fixed response size.
    let resp_size = probe_response_size(&addr).await?;
    println!("probe: response size = {resp_size} bytes");

    // Brief warmup: open all connections and do a few batches each.
    let counter = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    let mut handles = Vec::with_capacity(connections);
    for _ in 0..connections {
        let addr = addr.clone();
        let counter = Arc::clone(&counter);
        let stop = Arc::clone(&stop);
        handles.push(tokio::spawn(async move {
            if let Err(e) = connection_loop(addr, resp_size, counter, stop).await {
                eprintln!("conn error: {e}");
            }
        }));
    }

    // Warmup 2s (don't reset counter — noise is negligible at this rate).
    tokio::time::sleep(Duration::from_secs(2)).await;
    let start_count = counter.load(Ordering::Relaxed);
    let t0 = Instant::now();

    tokio::time::sleep(Duration::from_secs(duration_s)).await;

    let end_count = counter.load(Ordering::Relaxed);
    let elapsed = t0.elapsed().as_secs_f64();
    stop.store(true, Ordering::Relaxed);

    let done = end_count - start_count;
    let rps = done as f64 / elapsed;
    println!(
        "measured: {done} requests in {:.3}s → {:.0} req/s",
        elapsed, rps
    );

    // Give connections a moment to unwind.
    for h in handles {
        let _ = tokio::time::timeout(Duration::from_secs(1), h).await;
    }
    Ok(())
}
