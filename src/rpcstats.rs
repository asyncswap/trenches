// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs
//! What the RPC is doing for us, counted.
//!
//! Every network call is recorded here and every one is written to the trace
//! file. What reaches the event log is a **rollup**, once every 30 seconds:
//! rate, successes, failures, rate limits, and how long calls are taking.
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

use std::sync::atomic::{AtomicU64, Ordering};

static OK: AtomicU64 = AtomicU64::new(0);
static FAILED: AtomicU64 = AtomicU64::new(0);
static LIMITED: AtomicU64 = AtomicU64::new(0);
static MICROS: AtomicU64 = AtomicU64::new(0);
static SLOWEST: AtomicU64 = AtomicU64::new(0);
/// Unix seconds of the last rollup. Zero until the first call.
static SINCE: AtomicU64 = AtomicU64::new(0);

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

    if ok {
        OK.fetch_add(1, Ordering::Relaxed);
        crate::trace(&format!("rpc: {label} ok in {:.1}ms", micros as f64 / 1000.0));
        return;
    }

    FAILED.fetch_add(1, Ordering::Relaxed);
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
    let details = vec![
        ("rate", format!("{:.1}/s", total as f64 / elapsed.max(1) as f64)),
        ("ok", ok.to_string()),
        ("failed", if failed > 0 { failed.to_string() } else { String::new() }),
        ("rate_limited", if limited > 0 { limited.to_string() } else { String::new() }),
        ("avg", format!("{:.0}ms", micros as f64 / total as f64 / 1000.0)),
        ("slowest", format!("{:.0}ms", slowest as f64 / 1000.0)),
        ("over", format!("{elapsed}s")),
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
    match &out {
        Ok(_) => record(label, true, t0.elapsed(), None),
        Err(e) => record(label, false, t0.elapsed(), Some(&e.to_string())),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rate_limit_is_counted_apart_from_a_plain_failure() {
        let before = LIMITED.load(Ordering::Relaxed);
        record("x", false, std::time::Duration::from_millis(1), Some("HTTP 429 Rate Limit Hit"));
        assert_eq!(LIMITED.load(Ordering::Relaxed), before + 1);

        let mid = LIMITED.load(Ordering::Relaxed);
        record("x", false, std::time::Duration::from_millis(1), Some("connection reset"));
        assert_eq!(LIMITED.load(Ordering::Relaxed), mid, "not every failure is a rate limit");
    }

    #[test]
    fn a_success_is_counted_so_a_working_endpoint_is_visible() {
        let before = OK.load(Ordering::Relaxed);
        record("x", true, std::time::Duration::from_millis(5), None);
        assert_eq!(OK.load(Ordering::Relaxed), before + 1);
    }
}
