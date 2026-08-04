// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! The live launch feed: `eth_subscribe` on the launchpad factories.
//!
//! Polling cannot promise you saw every launch, and the reason is structural.
//! `getLogs` on a rate-limited endpoint is capped at ten blocks, so a round
//! covers about eighty — eight seconds of chain. When a round runs slow, an
//! endpoint answers 429, or the cursor falls behind, the scan window is
//! fast-forwarded to keep up with the head, and the blocks it skipped are never
//! read again. A launch in that gap is gone permanently, and nothing on screen
//! says so.
//!
//! A subscription has no window. The node pushes every matching log the moment
//! it is mined, so the only launches that can be missed are the ones that
//! happened while the socket was down — which is a knowable, bounded set, and
//! the poll below is what closes it.
//!
//! So this does NOT replace the scan. It runs beside it: the stream makes
//! launches appear immediately and covers the poll's gaps, the poll covers the
//! stream's disconnects, and the candidate lists dedupe by token so an event
//! arriving twice costs nothing. Two independent paths to the same fact is the
//! point, not redundancy to be optimised away.
//!
//! Lives exactly as long as the app: the task stops when `stop` is set, the
//! socket closes with it, and nothing runs in the background afterwards.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use alloy::rpc::types::Log;

use crate::lock;

/// The running stream, for anything that only wants to know whether it is up.
///
/// A global rather than a threaded parameter: the discovery title is drawn
/// from three call sites that have no other reason to know a websocket exists,
/// and passing one through all of them would be plumbing for a status light.
struct Live {
    /// Which chain this subscription is watching. A stream pointed at the
    /// chain you just left is worse than none: it delivers launches that
    /// belong to a different list.
    chain: u64,
    stream: LaunchStream,
    /// Owned here, not by the caller. The discovery SCREEN comes and goes; the
    /// feed is meant to outlive it and end with the app or with a chain
    /// switch, so its stop flag cannot belong to whoever happened to start it.
    stop: Arc<AtomicBool>,
}

static CURRENT: Mutex<Option<Live>> = Mutex::new(None);

/// What the discovery loop reads from.
#[derive(Clone, Default)]
pub struct LaunchStream {
    /// Logs delivered since the last drain. Bounded — see `push`.
    logs: Arc<Mutex<Vec<Log>>>,
    /// Whether a socket is currently up, for the status line.
    live: Arc<AtomicBool>,
    /// Rung on every push, so a consumer can SLEEP on the stream instead of
    /// polling its buffer. This is what turns "listening" into "streaming":
    /// without it, a launch sat in the buffer until the next round looked.
    notify: Arc<tokio::sync::Notify>,
}

impl LaunchStream {
    /// Take everything delivered since the last call.
    pub fn drain(&self) -> Vec<Log> {
        std::mem::take(&mut *lock(&self.logs))
    }

    /// Resolves when something has been pushed since the last drain.
    ///
    /// `Notify` holds one permit, so a push that lands while the consumer is
    /// mid-round is not lost — the next wait returns at once and the drain
    /// picks up everything.
    pub async fn wait(&self) {
        self.notify.notified().await;
    }

    fn push(&self, lg: Log) {
        let mut v = lock(&self.logs);
        // A drain that never happens must not grow without bound. Far more
        // than a round could ever produce, so this only bites if the consumer
        // has stopped — and then the poll is the one still covering us.
        if v.len() < 4_096 {
            v.push(lg);
            self.notify.notify_one();
        } else {
            crate::trace("launch stream: buffer full, dropped a log; the scan will recover it");
        }
    }
}

/// Subscribe to the launchpad factories over a websocket, forever, until
/// `stop`.
///
/// `addresses` and `topics` are passed in rather than imported so this file
/// stays about the transport: what counts as a launch belongs with the
/// decoders, in one place.
pub fn spawn(
    chain: u64,
    urls: Vec<String>,
    addresses: Vec<alloy::primitives::Address>,
    topics: Vec<alloy::primitives::B256>,
) -> LaunchStream {
    // ONE subscription per chain, however many times this is called.
    //
    // The discovery task starts each time that screen opens, so this used to
    // open a fresh socket on every visit while the previous one lived on.
    // Providers count connections, so a few trips in and out of the screen was
    // a few sockets against the same account — and the delivered counter
    // restarting at zero was the visible half of it.
    {
        let mut cur = lock(&CURRENT);
        match cur.as_ref() {
            // Same chain: the existing feed is the feed.
            Some(l) if l.chain == chain => return l.stream.clone(),
            // Different chain: stop the old one before starting another, or
            // switching chains would leave a socket behind on every switch.
            Some(l) => l.stop.store(true, Ordering::Relaxed),
            None => {}
        }
        *cur = None;
    }
    let stream = LaunchStream::default();
    let stop = Arc::new(AtomicBool::new(false));
    *lock(&CURRENT) =
        Some(Live { chain, stream: stream.clone(), stop: stop.clone() });
    if urls.is_empty() {
        // Said once, plainly. Without a websocket the app still works — it
        // just falls back to the polled scan, which cannot promise it saw
        // every launch, and that is worth knowing rather than inferring.
        crate::events::log(
            crate::events::Level::Warn,
            "No websocket configured, so launches are found by polling and some can be missed",
            &[("fix", "add a \"ws\" endpoint to this network in the config".into())],
        );
        return stream;
    }
    let task = stream.clone();
    tokio::spawn(async move {
        let stream = task;
        let mut attempt: u32 = 0;
        while !stop.load(Ordering::Relaxed) {
            // Round-robin across endpoints rather than hammering the first.
            // A provider having a bad minute should cost one reconnect, not
            // the whole session's feed.
            let url = urls[(attempt as usize) % urls.len()].clone();
            match run_once(&url, &addresses, &topics, &stream, &stop).await {
                Ok(()) => attempt = 0, // clean close: treat the next try as fresh
                Err(e) => {
                    attempt = attempt.saturating_add(1);
                    // Redacted: a websocket URL carries the API key in its path.
                    crate::trace(&format!(
                        "launch stream: {} (attempt {attempt})",
                        crate::net::redact(&e)
                    ));
                }
            }
            stream.live.store(false, Ordering::Relaxed);
            if stop.load(Ordering::Relaxed) {
                break;
            }
            // Backoff, capped. A socket that cannot connect must not become a
            // reconnect storm against an endpoint that is already struggling.
            let wait = std::time::Duration::from_millis(
                (500u64 << attempt.min(6)).min(30_000),
            );
            tokio::time::sleep(wait).await;
        }
        crate::trace("launch stream: closed");
    });
    stream
}

/// One connection's lifetime. Returns when the socket closes.
async fn run_once(
    url: &str,
    addresses: &[alloy::primitives::Address],
    topics: &[alloy::primitives::B256],
    stream: &LaunchStream,
    stop: &Arc<AtomicBool>,
) -> Result<(), String> {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let (mut ws, _) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio_tungstenite::connect_async(url),
    )
    .await
    .map_err(|_| "connect timed out".to_string())?
    .map_err(|e| format!("connect failed: {e}"))?;

    let sub = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_subscribe",
        "params": [
            "logs",
            {
                "address": addresses.iter().map(|a| format!("{a:#x}")).collect::<Vec<_>>(),
                // ONE topic slot holding the alternatives, not one slot each:
                // `[[a,b,c]]` means "topic0 is any of these", where
                // `[a,b,c]` would mean "topic0 is a AND topic1 is b" and match
                // nothing at all.
                "topics": [topics.iter().map(|t| format!("{t:#x}")).collect::<Vec<_>>()],
            }
        ],
    });
    ws.send(Message::Text(sub.to_string()))
        .await
        .map_err(|e| format!("subscribe failed: {e}"))?;

    let ack = tokio::time::timeout(std::time::Duration::from_secs(10), ws.next()).await;
    match ack {
        Err(_) => return Err("subscribe ack timed out".into()),
        Ok(None) => return Ok(()),
        Ok(Some(Err(e))) => return Err(format!("subscribe ack read failed: {e}")),
        Ok(Some(Ok(Message::Text(t)))) => {
            let v: serde_json::Value =
                serde_json::from_str(&t).map_err(|e| format!("subscribe ack unreadable: {e}"))?;
            if let Some(err) = v.get("error") {
                return Err(format!("subscribe refused: {err}"));
            }
            if let Some(result) = v.get("params").and_then(|p| p.get("result")) {
                if let Ok(lg) = serde_json::from_value::<Log>(result.clone()) {
                    stream.push(lg);
                }
            } else if v.get("result").is_none() {
                return Err(format!("subscribe ack unexpected: {t}"));
            }
        }
        Ok(Some(Ok(_))) => {}
    }
    stream.live.store(true, Ordering::Relaxed);
    crate::trace("launch stream: subscribed and acknowledged");

    while !stop.load(Ordering::Relaxed) {
        // A silent socket is indistinguishable from a dead one, and a dead one
        // that is never noticed is a feed that has stopped without saying so.
        // Robinhood Chain launches can be quiet for many minutes, so the read
        // has a long ceiling and a ping keeps the connection honest.
        let next = tokio::time::timeout(std::time::Duration::from_secs(90), ws.next()).await;
        let msg = match next {
            Err(_) => {
                ws.send(Message::Ping(Vec::new()))
                    .await
                    .map_err(|e| format!("ping failed: {e}"))?;
                continue;
            }
            Ok(None) => return Ok(()), // closed cleanly
            Ok(Some(Err(e))) => return Err(format!("read failed: {e}")),
            Ok(Some(Ok(m))) => m,
        };
        let t = match msg {
            Message::Text(t) => t,
            Message::Binary(b) => match String::from_utf8(b) {
                Ok(t) => t,
                Err(_) => continue,
            },
            Message::Ping(p) => {
                ws.send(Message::Pong(p)).await.map_err(|e| format!("pong failed: {e}"))?;
                continue;
            }
            Message::Close(_) => return Ok(()),
            _ => continue,
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) else { continue };
        // Subscription notifications only; the ack for our own request carries
        // an `id` and no params.
        let Some(result) = v.get("params").and_then(|p| p.get("result")) else { continue };
        match serde_json::from_value::<Log>(result.clone()) {
            Ok(lg) => stream.push(lg),
            // A log we cannot parse is a log we will still see on the next
            // poll, so it is worth a trace and not worth dropping the socket.
            Err(e) => crate::trace(&format!("launch stream: undecodable log: {e}")),
        }
    }
    Ok(())
}
