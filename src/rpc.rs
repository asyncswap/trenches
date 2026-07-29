// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! One transport that every EVM RPC request goes through.
//!
//! The app used to build one alloy provider on the primary URL and let every
//! caller fend for itself. The config's `rpcs` list promised "round-robin with
//! failover" and delivered it only on Solana; on EVM a 429 was logged, counted,
//! and then the same endpoint was asked again 350ms later. Once the public
//! RPC's per-minute window tripped, EVERYTHING — balances, prices, the head
//! block that discovery needs — failed together for a full minute.
//!
//! This module is a tower `Service` that alloy accepts as a transport, so the
//! typed contract calls, `get_balance`, `get_logs` — all of it — flow through
//! here without any caller changing. What it adds:
//!
//! - **Rotation with cooldowns.** Endpoints are tried in order of measured
//!   latency. A 429 puts the endpoint on cooldown for however long the reply
//!   says the window needs to reset (falling back to a default), and the
//!   request moves to the next endpoint instead of failing. A connection error
//!   or 5xx is a short cooldown: broken is usually brief.
//! - **Role-aware routing.** Alchemy's free tier caps `eth_getLogs` at a
//!   10-block range, so wide log scans are steered to endpoints that can
//!   answer them; everything else prefers whatever answers fastest.
//! - **A micro-cache for the chatter.** `eth_blockNumber`, `eth_gasPrice` and
//!   `eth_chainId` are asked constantly by independent loops that do not know
//!   about each other. Each gets a tiny TTL, so five pollers cost one request.
//! - **A per-endpoint concurrency cap.** Discovery bursts (12 pools in
//!   flight × several calls each) used to land on the node as one thundering
//!   herd. A semaphore smooths the burst into a queue the node will answer.
//!
//! What this deliberately does NOT do: hedged requests (racing two endpoints
//! doubles the spend that earned the 429s), and app-level caching of contract
//! state (that lives with the callers, who know what is immutable).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use alloy::rpc::json_rpc::{RequestPacket, Response, ResponsePacket, ResponsePayload};
use alloy::transports::{TransportError, TransportErrorKind, TransportFut};
use serde_json::value::RawValue;

/// Whole-request timeout, above whatever the caller wraps. An endpoint that
/// takes longer than this is worse than one that refused.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
/// In-flight requests per endpoint. Bursts queue here instead of at the node.
const PER_ENDPOINT_CONCURRENCY: usize = 8;
/// Cooldown for a 429 whose body doesn't say when the window resets.
const COOLDOWN_LIMITED: Duration = Duration::from_secs(15);
/// Cooldown for a connection error or 5xx — broken is usually brief.
const COOLDOWN_BROKEN: Duration = Duration::from_secs(3);
/// While EVERY eligible endpoint is benched, one is still probed — but at
/// most this often. Trying on every call kept a rate-limited endpoint's
/// quota window permanently saturated: pollers retried every 350ms, each
/// retry spent quota, and the "resets in 60 seconds" reset never came.
const PROBE_EVERY: Duration = Duration::from_secs(3);
/// The longest cooldown any reply can talk us into.
const COOLDOWN_MAX: Duration = Duration::from_secs(120);
/// Alchemy's free-tier `eth_getLogs` block-range cap. A scan wider than this
/// must go to an endpoint without the cap.
const NARROW_LOG_SPAN: u64 = 10;

/// TTLs for the three methods every loop asks over and over. Params-free, so
/// the method name alone is the cache key.
const CACHED: &[(&str, Duration)] = &[
    ("eth_blockNumber", Duration::from_millis(300)),
    ("eth_gasPrice", Duration::from_secs(5)),
    ("eth_chainId", Duration::from_secs(3600)),
];

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

struct Endpoint {
    url: reqwest::Url,
    /// Host only — never the full URL, which usually carries an API key.
    host: String,
    /// Can this endpoint answer a multi-thousand-block `eth_getLogs`?
    wide_logs: bool,
    /// Unix millis until which this endpoint is benched. 0 = available.
    cooldown_until: AtomicU64,
    /// Unix millis before which a BENCHED endpoint may not even be probed.
    next_probe: AtomicU64,
    /// Exponentially-weighted round-trip in micros, for ordering. Starts at 0,
    /// which sorts new endpoints first so each gets measured once.
    ewma_us: AtomicU64,
    sem: tokio::sync::Semaphore,
}

impl Endpoint {
    fn cooling(&self) -> bool {
        self.cooldown_until.load(Ordering::Relaxed) > now_ms()
    }
    fn bench(&self, d: Duration, why: &str) {
        let until = now_ms() + d.as_millis() as u64;
        let prev = self.cooldown_until.swap(until, Ordering::Relaxed);
        // Announce transitions, not every refused call — at hundreds of calls
        // a minute the repeated line is noise, the transition is news.
        if prev < now_ms() {
            crate::trace(&format!("rpc: {} benched {:.0}s ({why})", self.host, d.as_secs_f64()));
        }
    }
    fn observe(&self, elapsed: Duration) {
        let us = elapsed.as_micros() as u64;
        let prev = self.ewma_us.load(Ordering::Relaxed);
        let next = if prev == 0 { us } else { (prev * 7 + us) / 8 };
        self.ewma_us.store(next, Ordering::Relaxed);
    }
}

struct Inner {
    endpoints: Vec<Endpoint>,
    client: reqwest::Client,
    /// method -> (stored at, raw result). See CACHED for what qualifies.
    cache: Mutex<HashMap<&'static str, (Instant, Box<RawValue>)>>,
}

/// The balanced transport. Cheap to clone; all clones share state.
#[derive(Clone)]
pub struct Balanced(Arc<Inner>);

/// The session's pool, for callers too deep to be handed one — the raw JSON-RPC
/// batcher in discovery. Reset on every chain session, because each network has
/// its own endpoints. A `Mutex<Option<…>>`, not a `OnceLock`: sessions switch.
static SHARED: Mutex<Option<Balanced>> = Mutex::new(None);

pub fn set_shared(b: &Balanced) {
    *SHARED.lock().unwrap() = Some(b.clone());
}

pub fn shared() -> Option<Balanced> {
    SHARED.lock().unwrap().clone()
}

/// The RPC URLs a network offers. `rpc_pool()` already merges the `rpc` union
/// with the legacy `rpcs` / `discovery_rpc` fields and drops blanks, unfilled
/// placeholders and duplicates — this alias exists so call sites read as
/// "the URLs the transport balances over".
pub fn urls_for(net: &crate::config::Network) -> Vec<String> {
    net.rpc_pool()
}

impl Balanced {
    pub fn new(urls: &[String]) -> eyre::Result<Self> {
        let mut endpoints = Vec::new();
        for u in urls {
            let url: reqwest::Url = u
                .parse()
                .map_err(|e| eyre::eyre!("RPC URL {u:?} could not be parsed: {e}"))?;
            let host = url.host_str().unwrap_or("rpc").to_string();
            endpoints.push(Endpoint {
                // Alchemy free tier refuses wide getLogs ranges; everything
                // else is assumed able until it proves otherwise.
                wide_logs: !host.contains("alchemy"),
                host,
                url,
                cooldown_until: AtomicU64::new(0),
                next_probe: AtomicU64::new(0),
                ewma_us: AtomicU64::new(0),
                sem: tokio::sync::Semaphore::new(PER_ENDPOINT_CONCURRENCY),
            });
        }
        if endpoints.is_empty() {
            eyre::bail!("no usable RPC URLs configured");
        }
        let client = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|e| eyre::eyre!("HTTP client: {e}"))?;
        Ok(Self(Arc::new(Inner { endpoints, client, cache: Mutex::new(HashMap::new()) })))
    }

    /// One URL for callers that speak raw JSON-RPC themselves (the hand-rolled
    /// batch in discovery). Honors cooldowns and the wide-logs cap; falls back
    /// to the first endpoint rather than returning nothing.
    pub fn pick_url(&self, wide_logs: bool) -> String {
        let plan = self.0.plan(wide_logs);
        let i = plan.first().copied().unwrap_or(0);
        self.0.endpoints[i].url.to_string()
    }

    /// True when an endpoint that can serve WIDE `eth_getLogs` is rested.
    ///
    /// Pollers that scan logs every round must check this and SKIP the round
    /// when it is false. The transport's own fallback (try the benched
    /// endpoint when it is the only candidate) is right for a one-off call —
    /// but a loop that retries every second turns that mercy into a hammer,
    /// guaranteeing the endpoint never gets to finish resting.
    pub fn wide_ready(&self) -> bool {
        self.0.endpoints.iter().any(|e| e.wide_logs && !e.cooling())
    }

    /// Report the outcome of a raw call made against `pick_url`, so cooldowns
    /// and latency ordering learn from traffic that bypasses `call()`.
    pub fn report_raw(&self, url: &str, ok: bool, limited: bool, elapsed: Duration) {
        if let Some(ep) = self.0.endpoints.iter().find(|e| e.url.as_str() == url) {
            if limited {
                ep.bench(COOLDOWN_LIMITED, "429");
            } else if !ok {
                ep.bench(COOLDOWN_BROKEN, "error");
            } else {
                ep.observe(elapsed);
            }
        }
    }
}

impl Inner {
    /// Endpoint indices in the order they should be tried: eligible and rested
    /// first (fastest first), then the benched ones (soonest-free first) so a
    /// fully-benched pool still answers rather than starving.
    fn plan(&self, wide_logs: bool) -> Vec<usize> {
        let mut avail: Vec<usize> = Vec::new();
        let mut benched: Vec<usize> = Vec::new();
        for (i, ep) in self.endpoints.iter().enumerate() {
            if wide_logs && !ep.wide_logs {
                continue;
            }
            if ep.cooling() {
                benched.push(i);
            } else {
                avail.push(i);
            }
        }
        // Nothing can serve a wide scan: fall back to the full pool rather
        // than refusing — a capped endpoint returning an error beats silence.
        if avail.is_empty() && benched.is_empty() {
            return (0..self.endpoints.len()).collect();
        }
        avail.sort_by_key(|&i| self.endpoints[i].ewma_us.load(Ordering::Relaxed));
        benched.sort_by_key(|&i| self.endpoints[i].cooldown_until.load(Ordering::Relaxed));
        avail.extend(benched);
        avail
    }

    fn cache_ttl(method: &str) -> Option<(&'static str, Duration)> {
        CACHED.iter().find(|(m, _)| *m == method).copied()
    }

    fn cache_get(&self, key: &'static str, ttl: Duration) -> Option<Box<RawValue>> {
        let cache = self.cache.lock().unwrap();
        let (at, raw) = cache.get(key)?;
        (at.elapsed() < ttl).then(|| raw.clone())
    }

    fn cache_put(&self, key: &'static str, raw: Box<RawValue>) {
        self.cache.lock().unwrap().insert(key, (Instant::now(), raw));
    }

    async fn send(self: Arc<Self>, req: RequestPacket) -> Result<ResponsePacket, TransportError> {
        // What is being asked decides who gets asked.
        let (method, id) = match &req {
            RequestPacket::Single(r) => (r.method().to_string(), Some(r.id().clone())),
            RequestPacket::Batch(_) => ("batch".to_string(), None),
        };
        let wide = method == "eth_getLogs" && log_span(&req).is_none_or(|n| n > NARROW_LOG_SPAN);

        // The chatter cache: five loops polling the head block cost one call.
        let cached = Self::cache_ttl(&method);
        if let (Some((key, ttl)), Some(id)) = (cached, id.clone()) {
            if let Some(raw) = self.cache_get(key, ttl) {
                return Ok(ResponsePacket::Single(Response {
                    id,
                    payload: ResponsePayload::Success(raw),
                }));
            }
        }

        let body = serde_json::to_string(&req)
            .map_err(|e| TransportError::deser_err(e, "request serialization"))?;

        let plan = self.plan(wide);
        let mut last_err: Option<TransportError> = None;
        for (attempt, &i) in plan.iter().enumerate() {
            let ep = &self.endpoints[i];
            // A benched endpoint is only in the plan because everything better
            // already failed — and even then it takes one PROBE per few
            // seconds, not one per call. Pollers retry constantly; letting
            // each retry through kept the endpoint's quota window saturated
            // for as long as the poller ran.
            if ep.cooling() {
                let now = now_ms();
                if now < ep.next_probe.load(Ordering::Relaxed) {
                    last_err = Some(TransportErrorKind::custom_str(
                        "endpoint resting after a rate limit; retry shortly",
                    ));
                    continue;
                }
                ep.next_probe.store(now + PROBE_EVERY.as_millis() as u64, Ordering::Relaxed);
            }
            let _permit = ep.sem.acquire().await;
            let t0 = Instant::now();
            let resp = self
                .client
                .post(ep.url.clone())
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.clone())
                .send()
                .await;
            let resp = match resp {
                Ok(r) => r,
                Err(e) => {
                    ep.bench(COOLDOWN_BROKEN, "unreachable");
                    last_err = Some(TransportErrorKind::custom(e));
                    continue;
                }
            };
            let status = resp.status();
            let retry_after = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.trim().parse::<u64>().ok());
            let bytes = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    ep.bench(COOLDOWN_BROKEN, "body read failed");
                    last_err = Some(TransportErrorKind::custom(e));
                    continue;
                }
            };

            if status.as_u16() == 429 {
                let text = String::from_utf8_lossy(&bytes);
                ep.bench(limited_cooldown(&text, retry_after), "429");
                last_err =
                    Some(TransportErrorKind::http_error(429, text.into_owned()));
                continue;
            }
            if status.is_server_error() {
                ep.bench(COOLDOWN_BROKEN, "5xx");
                last_err = Some(TransportErrorKind::http_error(
                    status.as_u16(),
                    String::from_utf8_lossy(&bytes).into_owned(),
                ));
                continue;
            }
            if !status.is_success() {
                // A 4xx other than 429 is OUR mistake — every endpoint will
                // refuse it the same way, so asking the next one just spends
                // quota restating the problem.
                return Err(TransportErrorKind::http_error(
                    status.as_u16(),
                    String::from_utf8_lossy(&bytes).into_owned(),
                ));
            }

            let packet: ResponsePacket = match serde_json::from_slice(&bytes) {
                Ok(p) => p,
                Err(e) => {
                    return Err(TransportError::deser_err(e, String::from_utf8_lossy(&bytes)))
                }
            };

            // Some providers rate-limit with HTTP 200 and a JSON-RPC error.
            // That is still "this endpoint refused", not "this call is wrong".
            if packet_rate_limited(&packet) {
                let msg = packet
                    .as_error()
                    .map(|e| e.message.to_string())
                    .unwrap_or_else(|| "rate limited".into());
                ep.bench(limited_cooldown(&msg, retry_after), "429 in body");
                last_err = Some(TransportErrorKind::http_error(429, msg));
                continue;
            }

            ep.observe(t0.elapsed());
            if attempt > 0 {
                crate::trace(&format!("rpc: {} answered {method} after failover", ep.host));
            }
            if let (Some((key, _)), ResponsePacket::Single(r)) = (cached, &packet) {
                if let ResponsePayload::Success(raw) = &r.payload {
                    self.cache_put(key, raw.clone());
                }
            }
            return Ok(packet);
        }
        Err(last_err.unwrap_or_else(|| {
            TransportErrorKind::custom_str("no RPC endpoint could be tried")
        }))
    }
}

/// True when a parsed response is a provider refusing for rate, on any
/// endpoint's phrasing: code 429, the common -32005, or the words.
fn packet_rate_limited(packet: &ResponsePacket) -> bool {
    let is_limit = |e: &alloy::rpc::json_rpc::ErrorPayload| {
        e.code == 429
            || e.code == -32005
            || e.message.to_lowercase().contains("rate limit")
            || e.message.to_lowercase().contains("too many request")
    };
    match packet {
        ResponsePacket::Single(r) => r.payload.as_error().is_some_and(is_limit),
        // A batch is homogeneous in practice; one rate-limit error means the
        // endpoint is refusing, not that one sub-call was special.
        ResponsePacket::Batch(_) => {
            let mut errs = packet.iter_errors().peekable();
            errs.peek().is_some() && packet.iter_errors().any(is_limit)
        }
    }
}

/// How long a 429 should bench the endpoint: the Retry-After header if given,
/// else "reset in N seconds" parsed from the body, else the default. Capped —
/// no reply gets to bench an endpoint for five minutes.
fn limited_cooldown(body: &str, retry_after: Option<u64>) -> Duration {
    let secs = retry_after.or_else(|| {
        let lower = body.to_lowercase();
        let tail = lower.split("reset in ").nth(1)?;
        let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
        digits.parse().ok()
    });
    secs.map(Duration::from_secs).unwrap_or(COOLDOWN_LIMITED).min(COOLDOWN_MAX)
}

/// The block span of an `eth_getLogs`, if both ends are concrete numbers.
/// `None` means unbounded ("latest", "earliest", missing) — treated as wide.
fn log_span(req: &RequestPacket) -> Option<u64> {
    let RequestPacket::Single(r) = req else { return None };
    let v: serde_json::Value = serde_json::from_str(r.serialized().get()).ok()?;
    let f = v.get("params")?.get(0)?;
    let hex = |k: &str| -> Option<u64> {
        let s = f.get(k)?.as_str()?;
        u64::from_str_radix(s.trim_start_matches("0x"), 16).ok()
    };
    let (from, to) = (hex("fromBlock")?, hex("toBlock")?);
    Some(to.saturating_sub(from))
}

impl tower::Service<RequestPacket> for Balanced {
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: RequestPacket) -> Self::Future {
        let inner = self.0.clone();
        Box::pin(inner.send(req))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reset_hint_in_a_429_body_is_honored() {
        let d = limited_cooldown(
            r#"{"error":{"code":429,"message":"Rate Limit Hit, limit will reset in 60 seconds"}}"#,
            None,
        );
        assert_eq!(d, Duration::from_secs(60));
    }

    #[test]
    fn a_429_without_a_hint_gets_the_default_and_a_header_wins() {
        assert_eq!(limited_cooldown("Too Many Requests", None), COOLDOWN_LIMITED);
        assert_eq!(limited_cooldown("Too Many Requests", Some(7)), Duration::from_secs(7));
    }

    #[test]
    fn no_reply_can_bench_an_endpoint_past_the_cap() {
        let d = limited_cooldown("limit will reset in 86400 seconds", None);
        assert_eq!(d, COOLDOWN_MAX);
    }

    #[test]
    fn placeholder_and_blank_urls_are_dropped_not_dialed() {
        let net = crate::config::Network {
            rpc: vec!["https://primary.example/rpc".into()],
            rpcs: vec![
                "".into(),
                "https://robinhood-mainnet.g.alchemy.com/v2/YOUR_KEY".into(),
                "https://real.example/rpc".into(),
            ],
            discovery_rpc: Some("".into()),
            ..crate::config::Network::default()
        };
        let urls = urls_for(&net);
        assert!(urls.iter().all(|u| !u.contains("YOUR_") && !u.trim().is_empty()));
        assert_eq!(urls.first().map(String::as_str), Some("https://primary.example/rpc"));
        assert!(urls.iter().any(|u| u.contains("real.example")));
    }

    #[test]
    fn a_wide_scan_is_kept_off_capped_endpoints_a_narrow_one_is_not() {
        let b = Balanced::new(&[
            "https://robinhood-mainnet.g.alchemy.com/v2/abc".to_string(),
            "https://rpc.mainnet.chain.robinhood.com/rpc".to_string(),
        ])
        .unwrap();
        assert!(b.pick_url(true).contains("rpc.mainnet"));
        // Fresh endpoints have ewma 0; order between them is config order.
        assert!(b.pick_url(false).contains("alchemy"));
    }
}
