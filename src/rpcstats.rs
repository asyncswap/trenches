// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! What the RPC is doing for us, counted.
//!
//! Every network call is recorded here and every one is written to the trace
//! file. What reaches the event log is a **rollup**, once every 30 seconds:
//! rate, successes, failures, rate limits, and how long calls are taking.
//!
//! The recorder is the TRANSPORT (`src/rpc.rs`), so what is counted is what
//! actually went out on the wire — including the attempts that failed over to
//! a second endpoint. It used to be a handful of hand-wrapped call sites
//! instead, which counted about a dozen places and missed everything else:
//! the numbers looked like a tenth of the real traffic, so an endpoint being
//! hammered read as an endpoint with an absurdly low limit.
//!
//! That split is deliberate, and it is the same lesson as the poll stream. A
//! line per call means ten lines a second from market reads alone — which is
//! precisely the flood that made the log screen useless, and writing it as
//! `RPC ok` instead of `market: price=…` would not make it readable. Nobody can
//! see a rate by reading individual calls scroll past; a rate is the thing you
//! actually want to know, so the log states it.
//!
//! Success is acknowledged rather than assumed: the rollup says how many calls
//! answered, so a silent screen and a working endpoint are told apart without
//! having to infer it from the absence of errors.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

static OK: AtomicU64 = AtomicU64::new(0);
static FAILED: AtomicU64 = AtomicU64::new(0);
static LIMITED: AtomicU64 = AtomicU64::new(0);
static MICROS: AtomicU64 = AtomicU64::new(0);
static SLOWEST: AtomicU64 = AtomicU64::new(0);
/// Unix seconds of the last rollup. Zero until the first call.
static SINCE: AtomicU64 = AtomicU64::new(0);
/// Unix seconds of the most recent success / failure, for `health()`. The
/// counters above reset only at the rollup, which made the health light
/// sticky: one failure kept it amber for up to the whole window, no matter
/// how many clean answers followed.
static LAST_OK: AtomicU64 = AtomicU64::new(0);
static LAST_FAIL: AtomicU64 = AtomicU64::new(0);
/// Per-method tallies for the window: method -> (ok, failed). Which method is
/// spending the budget is the one thing that says what to cut, and a single
/// total cannot answer it — one wide `eth_getLogs` can cost a provider more
/// than a hundred `eth_call`s.
static METHODS: Mutex<BTreeMap<String, (u64, u64)>> = Mutex::new(BTreeMap::new());

/// These counters are process-global, so tests that assert an exact delta on
/// them have to run one at a time. Serialising is the honest fix: the
/// alternative is loosening the assertions to ">= before", which would stop
/// them catching a double count — exactly the bug that made the transport's
/// numbers wrong in the first place.
#[cfg(test)]
static TEST_GATE: Mutex<()> = Mutex::new(());

/// How often the rollup is written.
const WINDOW_SECS: u64 = 30;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Record one completed call.
///
/// `label` names what was asked for — it goes to the trace, which keeps the
/// per-call detail this deliberately does not put on screen.
pub fn record(label: &str, ok: bool, elapsed: std::time::Duration, err: Option<&str>) {
    let micros = elapsed.as_micros() as u64;
    MICROS.fetch_add(micros, Ordering::Relaxed);
    SLOWEST.fetch_max(micros, Ordering::Relaxed);
    SINCE.compare_exchange(0, now(), Ordering::Relaxed, Ordering::Relaxed).ok();

    if let Ok(mut m) = METHODS.lock() {
        let e = m.entry(label.to_string()).or_insert((0, 0));
        if ok { e.0 += 1 } else { e.1 += 1 }
    }

    if ok {
        OK.fetch_add(1, Ordering::Relaxed);
        LAST_OK.store(now(), Ordering::Relaxed);
        crate::trace(&format!("rpc: {label} ok in {:.1}ms", micros as f64 / 1000.0));
        return;
    }

    FAILED.fetch_add(1, Ordering::Relaxed);
    LAST_FAIL.store(now(), Ordering::Relaxed);
    let msg = err.unwrap_or("no reason given");
    // A rate limit is a different problem from a broken endpoint: one wants
    // patience or a paid key, the other wants a different URL. Counting them
    // apart is the difference between advice you can act on and "it failed".
    let limited = msg.contains("429") || msg.to_lowercase().contains("rate limit");
    if limited {
        LIMITED.fetch_add(1, Ordering::Relaxed);
    }
    crate::trace(&format!("rpc: {label} FAILED in {:.1}ms: {msg}", micros as f64 / 1000.0));
}

/// Write the rollup if the window has elapsed. Cheap; call it from a loop.
pub fn maybe_report() {
    let started = SINCE.load(Ordering::Relaxed);
    if started == 0 {
        return;
    }
    let elapsed = now().saturating_sub(started);
    if elapsed < WINDOW_SECS {
        return;
    }
    // Claim the window before reading the counters, so two callers cannot both
    // report it.
    if SINCE
        .compare_exchange(started, now(), Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }

    let ok = OK.swap(0, Ordering::Relaxed);
    let failed = FAILED.swap(0, Ordering::Relaxed);
    let limited = LIMITED.swap(0, Ordering::Relaxed);
    let micros = MICROS.swap(0, Ordering::Relaxed);
    let slowest = SLOWEST.swap(0, Ordering::Relaxed);

    let total = ok + failed;
    if total == 0 {
        return;
    }
    // The busiest methods, worst-offending first: what is actually being spent
    // on, and how much of it is being refused.
    let busiest = METHODS
        .lock()
        .map(|mut m| {
            let mut v: Vec<(String, (u64, u64))> = std::mem::take(&mut *m).into_iter().collect();
            v.sort_by_key(|(_, (ok, bad))| std::cmp::Reverse(ok + bad));
            v.into_iter()
                .take(3)
                .map(|(name, (ok, bad))| {
                    if bad > 0 { format!("{name} {}/{}", ok + bad, bad) } else { format!("{name} {}", ok + bad) }
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();

    let details = vec![
        ("rate", format!("{:.1}/s", total as f64 / elapsed.max(1) as f64)),
        ("ok", ok.to_string()),
        ("failed", if failed > 0 { failed.to_string() } else { String::new() }),
        ("rate_limited", if limited > 0 { limited.to_string() } else { String::new() }),
        ("avg", format!("{:.0}ms", micros as f64 / total as f64 / 1000.0)),
        ("slowest", format!("{:.0}ms", slowest as f64 / 1000.0)),
        ("over", format!("{elapsed}s")),
        // "method calls/refused" — the refused count is only shown when nonzero.
        ("busiest", busiest),
    ];
    // Levelled by what the numbers mean, so the reader does not have to do the
    // arithmetic: a rate limit is the one that explains an empty screen.
    if limited > 0 {
        crate::events::warn("RPC rate limited", &details);
    } else if failed > 0 {
        crate::events::warn("RPC calls failing", &details);
    } else {
        crate::events::info("RPC healthy", &details);
    }
}

/// How the endpoint is behaving right now, for a screen that wants to show it
/// rather than wait for the next rollup.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Health {
    /// Answering.
    Ok,
    /// Answering, but refusing some — usually a rate limit.
    Degraded,
    /// Nothing is getting through.
    Down,
}

/// What the last RECENT_SECS of traffic looked like. Cheap; call per frame.
///
/// Time-windowed rather than counter-based: the counters reset only at the
/// 30s rollup, so a single failure pinned the light amber for the rest of the
/// window even while every call succeeded — and on screens that never ran the
/// rollup, forever.
const RECENT_SECS: u64 = 10;

pub fn health() -> Health {
    let t = now();
    let ok_recent = t.saturating_sub(LAST_OK.load(Ordering::Relaxed)) <= RECENT_SECS;
    let fail_recent = t.saturating_sub(LAST_FAIL.load(Ordering::Relaxed)) <= RECENT_SECS;
    // Nothing attempted yet is not a fault. A screen that opens red before it
    // has asked anything teaches you to distrust the light.
    match (ok_recent, fail_recent) {
        (_, false) => Health::Ok,
        (true, true) => Health::Degraded,
        (false, true) => Health::Down,
    }
}

/// Time a call and record it. The value comes back untouched.
///
/// Takes `IntoFuture` rather than `Future`: alloy's builders (`get_balance`,
/// `.call()`) are not futures until awaited, so a plain `Future` bound rejects
/// exactly the calls worth timing.
pub async fn timed<T, E, F>(label: &str, fut: F) -> Result<T, E>
where
    F: std::future::IntoFuture<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    let t0 = std::time::Instant::now();
    let out = fut.await;
    // Traced with the caller's own name for the work — "balanceOf" reads better
    // in a trace than "eth_call". NOT counted: the transport counts every
    // request, and counting again here would double every call that reaches it.
    let ms = t0.elapsed().as_micros() as f64 / 1000.0;
    match &out {
        Ok(_) => crate::trace(&format!("call: {label} ok in {ms:.1}ms")),
        Err(e) => crate::trace(&format!("call: {label} FAILED in {ms:.1}ms: {e}")),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rate_limit_is_counted_apart_from_a_plain_failure() {
        let _gate = TEST_GATE.lock().unwrap_or_else(|e| e.into_inner());
        let before = LIMITED.load(Ordering::Relaxed);
        record("x", false, std::time::Duration::from_millis(1), Some("HTTP 429 Rate Limit Hit"));
        assert_eq!(LIMITED.load(Ordering::Relaxed), before + 1);

        let mid = LIMITED.load(Ordering::Relaxed);
        record("x", false, std::time::Duration::from_millis(1), Some("connection reset"));
        assert_eq!(LIMITED.load(Ordering::Relaxed), mid, "not every failure is a rate limit");
    }

    #[test]
    fn a_success_is_counted_so_a_working_endpoint_is_visible() {
        let _gate = TEST_GATE.lock().unwrap_or_else(|e| e.into_inner());
        let before = OK.load(Ordering::Relaxed);
        record("x", true, std::time::Duration::from_millis(5), None);
        assert_eq!(OK.load(Ordering::Relaxed), before + 1);
    }
}

#[cfg(test)]
mod method_tally_tests {
    use super::*;

    /// The rollup has to say WHICH method is spending the budget, and how much
    /// of that spend is being refused — a single total cannot tell you what to
    /// cut. Uniquely named so parallel tests cannot disturb the count.
    #[test]
    fn per_method_tallies_separate_answered_from_refused() {
        let _gate = TEST_GATE.lock().unwrap_or_else(|e| e.into_inner());
        let m = "eth_methodTallyFixture";
        record(m, true, std::time::Duration::from_millis(1), None);
        record(m, true, std::time::Duration::from_millis(1), None);
        record(m, false, std::time::Duration::from_millis(1), Some("HTTP 429 Rate Limit Hit"));

        let g = METHODS.lock().expect("tally lock");
        assert_eq!(g.get(m), Some(&(2, 1)), "two answered, one refused");
    }

    /// A refusal must land in BOTH the global rate-limit counter and the
    /// method's own tally, or the rollup would name a busy method while the
    /// headline said nothing was being limited.
    #[test]
    fn a_refusal_counts_in_both_places() {
        let _gate = TEST_GATE.lock().unwrap_or_else(|e| e.into_inner());
        let before = LIMITED.load(Ordering::Relaxed);
        record("eth_refusalFixture", false, std::time::Duration::from_millis(1), Some("rate limit"));
        assert!(LIMITED.load(Ordering::Relaxed) > before, "global rate-limit counter moved");
        let g = METHODS.lock().expect("tally lock");
        assert_eq!(g.get("eth_refusalFixture").map(|t| t.1), Some(1));
    }
}

