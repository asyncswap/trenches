// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! Robinhood Chain speed bot (Rust). Full feature port of the Zig engine,
//! built speed-first: concurrent reads, pre-flight gas protection, and ready
//! for a local Nitro node over ws:// or IPC (remote ~380ms -> local ~1ms).

mod agent;
mod verification;
mod config;
mod net;
mod contracts;
mod discover;
mod engine;
mod events;
mod token_metadata_chain_id;
mod ledger;
mod pnl;
mod pricing;
mod rpc;
mod rpcstats;
/// Solana / pump.fun adapter — compiled only with `--features solana`.
#[cfg(feature = "solana")]
mod sol;
mod ui;
mod update;
mod v3;
mod v4;
mod view;
mod wallet;

use zeroize::Zeroizing;

use std::collections::VecDeque;
use std::time::Duration;

use alloy::network::EthereumWallet;
use alloy::primitives::B256;
use alloy::providers::{Provider, ProviderBuilder};
use crossterm::{
    event::{Event, KeyCode},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::{prelude::*, widgets::*};

use config::Registry;
use engine::{Bot, Side};

/// Where the config was read from, for anything that needs to name it.
///
/// Nothing writes to it any more: discovered tokens go to the cache, so the
/// only file the app edits is one it owns. A config a user is told to edit
/// should not also be edited behind their back.
static REGISTRY_PATH: std::sync::OnceLock<String> = std::sync::OnceLock::new();


/// A selectable pool: either one we own (from the registry) or a public
/// real-world pool (added by key params, pool id computed).
#[derive(Clone)]
struct SelPool {
    label: String,
    kind: engine::PoolKind, // carries v4 pool_id or v3 pool_addr — no half-empty fields
    token: alloy::primitives::Address,
    sym: String,
    fee: u32,
    owned: bool,
    quote: engine::Quote,  // ETH or a stablecoin (type-safe, no ETH assumption)
    quote_sym: String,     // display symbol for the quote side
}

/// A v3 tick, as a price per token in dollars.
///
/// LP rows carried `tick [-887200, 204200]`, which is the pool's own
/// coordinate system: correct, and unreadable next to a column of dollars.
/// The bounds of a liquidity range are prices, and prices are the thing being
/// compared — so they are shown as prices.
///
/// A tick is a ratio of the pool's two RAW balances, `1.0001^tick`, so getting
/// back to a human price needs both which side is WETH and how many decimals
/// the token has. Neither is on the tape row; both are on the pool it belongs
/// to. Returns `None` for a venue whose orientation is not known here, rather
/// than printing a number derived from a guess.
fn tick_usd(bot: &Bot, tick: i32) -> Option<f64> {
    let engine::PoolKind::V3 { weth_is_token0, .. } = bot.pool.kind else {
        return None;
    };
    let quote_usd = bot.pool.quote_usd;
    if quote_usd <= 0.0 {
        return None;
    }
    // token1_raw per token0_raw.
    let ratio = 1.0001f64.powf(tick as f64);
    if !ratio.is_finite() || ratio <= 0.0 {
        return None;
    }
    // Put the memecoin on top, then undo the decimal scaling of both sides.
    let raw_tokens_per_quote = if weth_is_token0 { ratio } else { 1.0 / ratio };
    let scale = 1e18 / 10f64.powi(bot.pool.token_decimals as i32);
    let tokens_per_quote = raw_tokens_per_quote * scale;
    (tokens_per_quote > 0.0 && tokens_per_quote.is_finite())
        .then(|| quote_usd / tokens_per_quote)
}

/// Symbol for a non-ETH quote token.
///
/// Read from the facts cache first, because the quote side is not a short list
/// any more: a pons launch names its own pair asset, and those are already
/// arriving as tokenized equities rather than dollars. Calling an NVDA-quoted
/// pool "USD" prices every number on the screen in the wrong thing.
///
/// USDG stays hardcoded as a floor — it is the chain's default quote and must
/// resolve before any RPC has run. "USD" is now only what an unknown token
/// falls back to once the cache has nothing, and it is deliberately vague
/// rather than confidently wrong.
fn stable_symbol(addr: alloy::primitives::Address) -> String {
    if let Some(f) = crate::token_metadata_chain_id::get(addr) {
        if !f.sym.is_empty() {
            return f.sym;
        }
    }
    match format!("{addr:#x}").as_str() {
        "0x5fc5360d0400a0fd4f2af552add042d716f1d168" => "USDG".to_string(),
        _ => "USD".to_string(),
    }
}

/// CoinGecko coin id for a stablecoin quote token, for the live USD fetch.
fn stable_cg_id(addr: alloy::primitives::Address) -> Option<&'static str> {
    match format!("{addr:#x}").as_str() {
        "0x5fc5360d0400a0fd4f2af552add042d716f1d168" => Some("global-dollar"),
        _ => None,
    }
}

/// ERC-20 decimals for a known stablecoin quote (USDG is 6-dec, not 18).
/// Fallback 18. TODO: read on-chain for arbitrary stables.
fn stable_decimals(addr: alloy::primitives::Address) -> u8 {
    match format!("{addr:#x}").as_str() {
        "0x5fc5360d0400a0fd4f2af552add042d716f1d168" => 6, // USDG
        _ => 18,
    }
}

/// Live USD value of one unit of a quote currency, from the CoinGecko feed.
/// Stables fall back to $1 only until the first fetch lands — never permanently.
fn quote_usd_of(quote: engine::Quote, eth_usd: f64, prices: &std::collections::HashMap<String, f64>) -> f64 {
    match quote {
        engine::Quote::Eth => eth_usd,
        engine::Quote::Stable { token, .. } => stable_cg_id(token)
            .and_then(|id| prices.get(id).copied())
            .unwrap_or(1.0),
    }
}

/// Push freshly-fetched USD prices into the bot: ETH and each pool's quote.
fn apply_prices(bot: &mut Bot, prices: &std::collections::HashMap<String, f64>) {
    if let Some(&e) = prices.get("ethereum") {
        if e > 0.0 {
            bot.eth_usd = e;
        }
    }
    let eu = bot.eth_usd;
    bot.pool.quote_usd = quote_usd_of(bot.pool.quote, eu, prices);
    if let Some(pb) = bot.pool_b.as_mut() {
        pb.quote_usd = quote_usd_of(pb.quote, eu, prices);
    }
}

/// CoinGecko ids to fetch for a set of pools: ETH plus every stable quote.
fn price_ids(pools: &[SelPool]) -> Vec<&'static str> {
    let mut ids = vec!["ethereum"];
    for p in pools {
        if let engine::Quote::Stable { token, .. } = p.quote {
            if let Some(id) = stable_cg_id(token) {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
    }
    ids
}

/// Remember the last pool used, per chain, so the next session starts on it.
/// The wallet unlocked last, so it can be offered first next time.
fn last_wallet_path() -> String {
    format!("{}/last-wallet.txt", state_dir())
}

fn load_last_wallet() -> Option<String> {
    std::fs::read_to_string(last_wallet_path())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// How much one press of `[` or `]` moves the buy size.
///
/// A fixed 0.5% was right on the wallet this was written for and useless on a
/// larger one: the step is a fraction of the balance, so the bigger the
/// balance the bigger the smallest adjustment you can make. Past a few
/// thousand dollars the finest available change is larger than the whole
/// position a smaller account would take, which is the opposite of what
/// precision should do.
///
/// So the step gets finer as the wallet grows. Two tiers, not a formula: a
/// step that slides continuously with the balance means the same key does a
/// different thing every session, and a size control has to be predictable
/// before it is clever.
fn buy_step(bot: &engine::Bot) -> f64 {
    if let Some(s) = bot.buy_step_override {
        return s; // you said; that settles it
    }
    let wallet_usd = bot.eth * bot.eth_usd;
    // With no USD feed there is nothing to judge "large" against; keep the
    // step that has always been there rather than guessing from raw ETH.
    if wallet_usd >= 1_000.0 { 0.001 } else { 0.005 }
}

/// The steps `;` and `'` move between, coarsest last.
///
/// A ladder rather than a multiplier: every rung is a number people already
/// think in, and doubling from 0.5% would land on 0.8% and 1.6%, which nobody
/// has ever wanted a trade size to be.
const BUY_STEPS: [f64; 5] = [0.0001, 0.001, 0.005, 0.01, 0.05];

/// Move the buy-size step one rung, and remember that you chose.
fn nudge_buy_step(bot: &mut engine::Bot, coarser: bool) {
    let now = buy_step(bot);
    // Start from the rung nearest what is in effect, so the first press moves
    // from where you are rather than from where the ladder happens to begin.
    let i = BUY_STEPS
        .iter()
        .enumerate()
        .min_by(|a, b| {
            (a.1 - now).abs().partial_cmp(&(b.1 - now).abs()).unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(i, _)| i)
        .unwrap_or(2);
    let next = if coarser {
        (i + 1).min(BUY_STEPS.len() - 1)
    } else {
        i.saturating_sub(1)
    };
    bot.buy_step_override = Some(BUY_STEPS[next]);
    // A finer step is pointless if the size cannot sit on it, and a coarser one
    // must not leave the size off its own grid.
    let step = BUY_STEPS[next];
    bot.buy_frac = ((bot.buy_frac / step).round() * step).clamp(step, 1.0);
    let msg = buy_size_status(bot);
    setting(bot, "Buy step", pct_compact(step), msg);
}

/// A percentage with as few decimals as it needs: `5%`, `0.5%`, `0.1%`.
///
/// One decimal cannot show a 0.1% step landing on 1.25%, and two decimals
/// print `5.00%` for every ordinary size. Trim what is not carrying meaning.
fn pct_compact(frac: f64) -> String {
    let p = frac * 100.0;
    if (p * 10.0).fract().abs() > 1e-6 {
        format!("{p:.2}%")
    } else if p.fract().abs() > 1e-6 {
        format!("{p:.1}%")
    } else {
        format!("{p:.0}%")
    }
}


#[cfg(test)]
mod buy_size_tests {
    use super::pct_compact;

    #[test]
    fn a_percentage_shows_only_the_decimals_it_needs() {
        assert_eq!(pct_compact(0.05), "5%");
        assert_eq!(pct_compact(0.005), "0.5%");
        // The case one decimal could not show: a 0.1% step off a 1.2% size.
        assert_eq!(pct_compact(0.0125), "1.25%");
        assert_eq!(pct_compact(0.001), "0.1%");
    }
}

/// The buy-size status, quoting the stake in the units that matter: the
/// percent you set, the ETH it works out to, and the dollars that is.
fn buy_size_status(bot: &engine::Bot) -> String {
    let stake = bot.eth * bot.buy_frac;
    let usd = stake * bot.eth_usd;
    if bot.eth > 0.0 && bot.eth_usd > 0.0 {
        format!(
            "Buy size {} \u{2248} {} ETH ({}) \u{b7} [ ] move by {}",
            pct_compact(bot.buy_frac),
            view::eth(stake),
            view::usd_compact(usd),
            pct_compact(buy_step(bot))
        )
    } else if bot.eth > 0.0 {
        format!("Buy size {} \u{2248} {} ETH", pct_compact(bot.buy_frac), view::eth(stake))
    } else {
        format!("Buy size is now {} of your ETH balance", pct_compact(bot.buy_frac))
    }
}

/// Where a token's tape history sleeps between sessions. Keyed by TOKEN, so
/// the history follows the coin across venue upgrades and pool migrations.
pub(crate) fn evm_tape_path(token: &alloy::primitives::Address) -> String {
    format!("{}/tape-evm-{token}.jsonl", state_dir())
}

/// Persist the tape, newest last, capped — a restart should reopen a coin
/// onto the same tape it left, not an empty room waiting for strangers.
fn save_evm_tape(token: &alloy::primitives::Address, rows: &std::collections::VecDeque<engine::Swap>) {
    if token.is_zero() {
        return;
    }
    let _ = std::fs::create_dir_all(state_dir());
    let mut out = String::new();
    for s in rows.iter() {
        if let Ok(j) = serde_json::to_string(s) {
            out.push_str(&j);
            out.push('\n');
        }
    }
    let path = evm_tape_path(token);
    let tmp = format!("{path}.tmp");
    if std::fs::write(&tmp, out).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn load_evm_tape(token: &alloy::primitives::Address) -> std::collections::VecDeque<engine::Swap> {
    let mut out = std::collections::VecDeque::new();
    if token.is_zero() {
        return out;
    }
    if let Ok(s) = std::fs::read_to_string(evm_tape_path(token)) {
        for line in s.lines() {
            if let Ok(sw) = serde_json::from_str::<engine::Swap>(line) {
                out.push_back(sw);
            }
        }
    }
    // Retire placeholders whose real log has since landed. A confirmed order is
    // injected as a placeholder (eth_wei == 0, price rebuilt from order-time
    // facts) so your own fill shows even when a throttled endpoint answers the
    // window thinly — but the merge used to key on eth_wei, so the real log
    // never matched it and ONE swap persisted as two rows at different prices.
    // Saved tapes still carry those pairs; drop the placeholder side on load.
    //
    // ALL of them, not just the ones a real log has caught up with. A
    // placeholder is a within-session patch over a thin `getLogs` window, and
    // the ones already on disk were stamped with whatever block was current
    // when they were written — so reloading a token put its old fills at the
    // top of the tape dated seconds ago. They are re-injected from the order
    // record a moment later, at the block the trade actually landed in, so
    // dropping them here loses nothing and repairs every tape already saved.
    out.retain(|s| s.eth_wei != 0);
    // A row with no block cannot be placed in time at all: age is measured as
    // distance from the head, so block 0 reads as the age of the chain. Two
    // such rows in the USDG tape dated a 22-hour-old swap at 28 days.
    out.retain(|s| s.block > 0);
    while out.len() > 400 {
        out.pop_front();
    }
    out
}

fn save_last_wallet(name: &str) {
    let _ = std::fs::create_dir_all(state_dir());
    let _ = std::fs::write(last_wallet_path(), name);
}

/// The chain used last, remembered across runs.
///
/// Stored by NAME rather than by index: the config is a file people edit, and an
/// index would silently point at a different chain the moment a line moved.
fn last_chain_path() -> String {
    format!("{}/last-chain.txt", state_dir())
}

fn load_last_chain() -> Option<String> {
    std::fs::read_to_string(last_chain_path())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn save_last_chain(name: &str) {
    let _ = std::fs::create_dir_all(state_dir());
    let _ = std::fs::write(last_chain_path(), name);
}

/// The UI heartbeat, process-wide: EVERY loop that draws a screen — the
/// dashboard, discovery, docs, pickers, the Solana app — beats it each pass.
/// The freeze watchdog reads it, so "the screen is not updating" means any
/// screen, and a modal that is happily drawing itself is not a false alarm.
static UI_BEAT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn ui_epoch() -> &'static std::time::Instant {
    static T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    T0.get_or_init(std::time::Instant::now)
}

/// Called from every screen's draw loop: "a frame just went out".
pub fn ui_alive() {
    UI_BEAT.store(ui_epoch().elapsed().as_millis() as u64, std::sync::atomic::Ordering::Relaxed);
}

fn ui_beat_ms() -> u64 {
    UI_BEAT.load(std::sync::atomic::Ordering::Relaxed)
}

/// What the UI thread is doing right now, so a freeze can be NAMED rather than
/// just counted. Process-wide for the same reason the heartbeat is: the freeze
/// watchdog used to live inside the EVM dashboard, which meant a hang anywhere
/// else — the Solana app, discovery, a picker — produced no record at all, and
/// "it froze" was the entire bug report.
static UI_PHASE: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

pub fn ui_phase_set(p: &str) {
    let mut g = lock(&UI_PHASE);
    g.clear();
    g.push_str(p);
}

fn ui_phase_get() -> String {
    lock(&UI_PHASE).clone()
}

/// Watch the UI heartbeat and record any freeze, wherever it happened.
///
/// Spawned once for the process. Reads the phase WHILE stuck — reading it after
/// recovery named whatever ran next instead (always "idle", which explained
/// nothing).
pub fn spawn_ui_watchdog() -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut stalled: u64 = 0;
        let mut own_gap: u64 = 0;
        let mut held = String::new();
        let mut last_wake = ui_epoch().elapsed().as_millis() as u64;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            let now = ui_epoch().elapsed().as_millis() as u64;
            // If the watchdog ITSELF skipped time, the whole process was paused
            // — system sleep, a stopped terminal, a debugger — and no operation
            // in the app is to blame.
            own_gap = own_gap.max(now.saturating_sub(last_wake));
            last_wake = now;
            let gap = now.saturating_sub(ui_beat_ms());
            if gap >= 1_000 {
                if stalled < 1_000 {
                    held = ui_phase_get();
                }
                stalled = gap;
                continue;
            }
            if stalled >= 1_000 {
                // Logged on recovery, with the whole duration — one line per
                // freeze, not one per watchdog tick.
                let secs = format!("{:.1}s", stalled as f64 / 1000.0);
                if own_gap * 2 >= stalled {
                    trace(&format!("ui: whole process paused {secs} (sleep/suspend)"));
                    events::info(
                        "The whole app was paused — system sleep or a suspended terminal, not a slow operation",
                        &[("for", secs)],
                    );
                } else {
                    let during = if held.is_empty() { "unlabelled".to_string() } else { held.clone() };
                    trace(&format!("ui: stalled {secs} in {during}"));
                    events::warn(
                        "The screen froze because a slow operation held the UI thread",
                        &[("for", secs), ("during", during)],
                    );
                }
            }
            stalled = 0;
            own_gap = 0;
        }
    })
}

/// Delete all but the newest KEEP_LOGS of each per-session log family
/// (`session-*.log`, `evm-trace-*.log`, `sol-trace-*.log`). The timestamps in
/// the names sort lexically, so "newest" is a sort, not a stat.
fn prune_session_logs() {
    const KEEP_LOGS: usize = 20;
    let Ok(dir) = std::fs::read_dir(state_dir()) else { return };
    let mut families: std::collections::HashMap<&str, Vec<std::path::PathBuf>> =
        std::collections::HashMap::new();
    for entry in dir.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        for fam in ["session-", "evm-trace-", "sol-trace-"] {
            if name.starts_with(fam) && name.ends_with(".log") {
                families.entry(fam).or_default().push(entry.path());
            }
        }
    }
    for (_, mut paths) in families {
        paths.sort();
        for p in paths.iter().rev().skip(KEEP_LOGS) {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Append a diagnostic line to this session's trace.
///
/// Separate from the trading log: that records what was traded, this records
/// what the app BELIEVED — decimals, quote currency, token ordering. Every
/// pricing bug so far has come from one of those being wrong, and none of them
/// were visible on screen until the number was already nonsense.
/// Lock a mutex, recovering from poisoning rather than panicking.
///
/// Every mutex these wrap guards a CACHE — RPC facts, the shared endpoint pool,
/// the event ring. If some thread panicked while holding one, the worst case is
/// stale data; but `.lock().unwrap()` turns that single panic into a
/// process-wide kill switch that fires on the NEXT call, and this app can be
/// holding an open position when it does. Take the data and carry on.
/// The session log's filename, so the Logs panel can name the file someone is
/// about to be asked to attach to a bug report. Reading a screen and then
/// hunting `~/.trenches` for which of twenty files it was is a step nobody
/// should have to take.
static SESSION_LOG: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// The chain this session signs on, for anything keyed per chain.
///
/// Set once, from the id the ENDPOINT reports, after that has been checked
/// against the config — so a cache is never keyed by a chain the app only
/// believed it was on.
static CHAIN_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn set_chain_id(id: u64) {
    CHAIN_ID.store(id, std::sync::atomic::Ordering::Relaxed);
}

pub fn chain_id() -> u64 {
    CHAIN_ID.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn set_session_log(path: &str) {
    let name = std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut g = lock(&SESSION_LOG);
    g.clear();
    g.push_str(&name);
}

/// `session-….log`, or empty before one exists.
pub fn session_log_name() -> String {
    lock(&SESSION_LOG).clone()
}

pub fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Seconds east of UTC for the local timezone.
///
/// Read once from the OS. Orders are stamped in unix seconds — the only form
/// that survives a restart and sorts correctly — but read back on the wall
/// clock, because "was that me, ten minutes ago?" is a question about the room

pub fn trace(msg: &str) {
    use std::io::Write;
    use std::sync::{Mutex, OnceLock};
    // The trace file is what people attach to bug reports, and a transport
    // error arrives here with the failing URL — key and all — still in it.
    // Cheap check first: the hot polling lines carry no URL, and this runs
    // about ten times a second.
    let owned;
    let msg: &str = if msg.contains("://") {
        owned = crate::net::redact(msg);
        &owned
    } else {
        msg
    };
    struct Trace {
        w: std::io::BufWriter<std::fs::File>,
        last_flush: std::time::Instant,
    }
    static FILE: OnceLock<Option<Mutex<Trace>>> = OnceLock::new();
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    let start = START.get_or_init(std::time::Instant::now);
    let f = FILE.get_or_init(|| {
        std::fs::create_dir_all(state_dir()).ok()?;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(format!("{}/evm-trace-{ts}.log", state_dir()))
            .ok()
            .map(|f| Mutex::new(Trace { w: std::io::BufWriter::new(f), last_flush: std::time::Instant::now() }))
    });
    if let Some(f) = f {
        if let Ok(mut t) = f.lock() {
            let _ = writeln!(t.w, "{:8.3}  {msg}", start.elapsed().as_secs_f64());
            // Flush at most every 250ms. This used to flush on EVERY line,
            // under a global lock, from the hottest loops in the app — a
            // synchronous disk write per RPC call, dominating the very
            // latencies the trace was recording. A quarter-second tail is an
            // acceptable price; a crash loses at most that much history.
            if t.last_flush.elapsed().as_millis() >= 250 {
                t.last_flush = std::time::Instant::now();
                let _ = t.w.flush();
            }
        }
    }

    // And keep a copy in memory, stamped the same way the engine stamps its own
    // lines, so `l` shows it. Discovery and pool resolution only ever wrote to
    // the trace file, so the one screen someone opens when something looks
    // broken was the one place their failures did not appear.
    //
    // But only what a person would want to read. The trace file records every
    // poll — `market:` alone lands about ten times a second — and putting that
    // on screen buried the handful of lines that meant something under
    // thousands of identical ones. The file still gets everything; the screen
    // gets events.
    const POLLING: [&str; 5] = ["market:", "pool ", "tape:", "ui:", "rpc:"];
    if POLLING.iter().any(|p| msg.starts_with(p)) {
        return;
    }
    if let Ok(mut ring) = diagnostics().lock() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let (h, m, sec) = ((now % 86400) / 3600, (now % 3600) / 60, now % 60);
        // A repeat says nothing the previous line did not. A scan that fails
        // the same way every round should read as one fact, not a wall.
        if ring.back().is_some_and(|l| l.get(11..) == Some(msg)) {
            return;
        }
        ring.push_back(format!("[{h:02}:{m:02}:{sec:02}] {msg}"));
        while ring.len() > 500 {
            ring.pop_front();
        }
    }
}

/// In-memory copy of the trace lines, for the log screen.
pub fn diagnostics() -> &'static std::sync::Mutex<std::collections::VecDeque<String>> {
    static RING: std::sync::OnceLock<std::sync::Mutex<std::collections::VecDeque<String>>> =
        std::sync::OnceLock::new();
    RING.get_or_init(Default::default)
}

/// Everything the pricing math depends on, in one line, whenever a pool loads.
pub fn trace_pool(where_: &str, p: &engine::PoolCfg) {
    trace(&format!(
        "pool {where_}: sym={} quote={} quote_dec={} token_dec={} kind={} token={:#x} fee={}",
        p.sym,
        p.quote_sym,
        p.quote.decimals(),
        p.token_decimals,
        p.kind.proto(),
        p.token,
        p.fee,
    ));
}

/// Directory for everything this bot writes: session logs, the daily PnL file,
/// the saved theme, the last pool. One constant so a rename cannot leave half
/// the app writing to the old place.
/// Everything this bot writes: session logs, traces, the theme, cached tokens,
/// the fill ledger, the daily PnL baseline.
///
/// Resolved once, and ABSOLUTE. It used to be the literal `".trenches"`, which
/// is relative to whatever directory the shell happened to be in — so a binary
/// on your PATH scattered a fresh, empty state directory everywhere it was run
/// from, and a PnL calendar opened from the wrong folder found no trades because
/// they were written somewhere else. Every doc already said `~/.trenches`.
///
/// A `./.trenches` that already exists still wins, so a checkout that has been
/// accumulating logs keeps them.
pub fn state_dir() -> &'static str {
    static DIR: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let env = std::env::var("TRENCHES_STATE").ok().filter(|v| !v.is_empty());
        let home = config::home_dir().map(|h| h.join(".trenches").to_string_lossy().into_owned());
        resolve_state_dir(env, home)
    })
}

/// Where history lives: `$TRENCHES_STATE`, else `~/.trenches`.
///
/// It used to prefer a `.trenches` in the working directory, which meant your
/// PnL depended on where you happened to launch from — two shells, two
/// different calendars, and nothing on screen saying which one you were looking
/// at. Fine while the only user ran it from a checkout; a trap for anyone
/// running a release from a project folder that happens to contain one.
///
/// The local directory is still available, but you have to ask for it by name.
fn resolve_state_dir(env: Option<String>, home: Option<String>) -> String {
    // Last resort only: with no home directory there is nowhere better, and
    // losing the history outright is worse than putting it underfoot.
    env.or(home).unwrap_or_else(|| ".trenches".to_string())
}

#[cfg(test)]
mod state_dir_tests {
    use super::resolve_state_dir;

    #[test]
    fn history_does_not_depend_on_where_you_launched_from() {
        let home = Some("/home/me/.trenches".to_string());
        // The same answer whatever the working directory holds — that is the
        // whole point of the change.
        assert_eq!(resolve_state_dir(None, home.clone()), "/home/me/.trenches");
        // Asked for explicitly, it is honoured.
        assert_eq!(
            resolve_state_dir(Some("/tmp/scratch".into()), home.clone()),
            "/tmp/scratch"
        );
        // An empty variable is not an answer; it is a variable someone forgot
        // to set, and it must not send the history to "".
        assert_eq!(resolve_state_dir(None, home), "/home/me/.trenches");
    }

    #[test]
    fn with_no_home_it_falls_back_rather_than_losing_the_history() {
        assert_eq!(resolve_state_dir(None, None), ".trenches");
    }
}

/// Read an ERC-20 symbol on-chain (fallback "TOK").
async fn read_symbol<P: Provider>(provider: &P, token: alloy::primitives::Address) -> String {
    contracts::IERC20::new(token, provider)
        .symbol()
        .call()
        .await
        .map(|s| s._0)
        .unwrap_or_else(|_| "TOK".to_string())
}

/// Auto-find a token's WETH v3 pool: scan the fee tiers and return the first with
/// live liquidity as (pool_addr, fee, weth_is_token0). None if the token has no
/// liquid v3 pool.
async fn find_v3_pool<P: Provider>(
    provider: &P,
    token: alloy::primitives::Address,
) -> Option<(alloy::primitives::Address, u32, bool)> {
    let factory = contracts::IV3Factory::new(contracts::V3_FACTORY, provider);
    for fee in [10000u32, 3000, 500, 100] {
        if let Ok(p) = factory.getPool(token, contracts::WETH, fee.try_into().unwrap()).call().await {
            let addr = p.pool;
            if addr != alloy::primitives::Address::ZERO {
                let liq = contracts::IV3Pool::new(addr, provider)
                    .liquidity()
                    .call()
                    .await
                    .map(|l| l._0)
                    .unwrap_or(0);
                if liq > 0 {
                    return Some((addr, fee, contracts::WETH < token));
                }
            }
        }
    }
    None
}

/// Candidate ETH-quoted venues for a token — the routes a buy/sell is simulated
/// across for best execution. Only ETH-quoted pools qualify (proceeds in ETH).
fn routes_for(pools: &[SelPool], token: alloy::primitives::Address) -> Vec<engine::Route> {
    pools
        .iter()
        .filter(|p| p.token == token && p.quote.is_eth())
        .map(|p| engine::Route { kind: p.kind, token: p.token, fee: p.fee, label: fee_label(p.fee) })
        .collect()
}

/// Derive the quote currency from a pool's currency0. ETH pools use ZERO/WETH;
/// anything else is a stablecoin quote (priced live, never assumed $1).
fn quote_from(currency0: &str) -> (engine::Quote, String) {
    let addr = currency0.parse::<alloy::primitives::Address>().unwrap_or(alloy::primitives::Address::ZERO);
    // flETH counts as ETH: it is redeemable 1:1, so a Flaunch pool is
    // ETH-quoted even though its currency0 is the wrapper.
    if addr == alloy::primitives::Address::ZERO || addr == contracts::WETH || addr == contracts::FLETH {
        (engine::Quote::Eth, "ETH".to_string())
    } else {
        (engine::Quote::Stable { token: addr, decimals: stable_decimals(addr) }, stable_symbol(addr))
    }
}

/// What the header wears: the mark and the name beside it.
///
/// Most specific first — a launchpad beats the AMM it graduates into, which
/// beats the chain underneath. With no pool selected there is no venue at all,
/// so it falls all the way back to the chain.
/// Fill the venue signals for the selected pool — Pons socials + graduation
/// block, or the Flaunch metadata + launch block when the pool is a Flaunch
/// one. Kind-gated so a Flaunch launch block can never make a plain Uniswap
/// pool wear the Pons mark (the Pons signal is `pool_launch_block` alone).
async fn refresh_venue_meta<P: Provider>(provider: &P, bot: &mut Bot) {
    if matches!(bot.pool.kind, engine::PoolKind::FlaunchV4 { .. }) {
        // The facts cache answers first — a coin picked from discovery (or
        // revisited) has its launch block and IPFS metadata on disk already,
        // so re-selecting it costs nothing.
        if let Some(f) = token_metadata_chain_id::get(bot.pool.token) {
            if f.launch_block.unwrap_or(0) > 0 && !f.socials.is_empty() {
                bot.pool_launch_block = f.launch_block;
                bot.socials = f.socials;
                return;
            }
        }
        match discover::fetch_flaunch_pool(provider, bot.pool.token).await {
            Some(fl) => {
                bot.pool_launch_block = Some(fl.launch_block);
                bot.socials = engine::fetch_flaunch_meta(&fl.token_uri).await;
                let (meta, block) = (bot.socials.clone(), fl.launch_block);
                token_metadata_chain_id::merge(bot.pool.token, move |f| {
                    if block > 0 {
                        f.launch_block = Some(block);
                    }
                    if !meta.is_empty() {
                        f.socials = meta;
                    }
                });
            }
            None => {
                bot.pool_launch_block = None;
                bot.socials = Default::default();
            }
        }
    } else {
        bot.socials = token_metadata_chain_id::ensure(provider, bot.pool.token, None).await.socials;
        bot.pool_launch_block = token_metadata_chain_id::launch_block(provider, bot.pool.token).await;
    }
}

fn header_venue(bot: &Bot) -> ui::image::Venue {
    if bot.pool.token.is_zero() {
        ui::image::Venue::Chain
    } else if matches!(bot.pool.kind, engine::PoolKind::FlaunchV4 { .. }) {
        // The kind IS the signal — checked before pons_launch so a launch
        // block set for the Age row can never relabel a Flaunch coin.
        ui::image::Venue::Flaunch
    } else if bot.pons_launch().is_some() {
        ui::image::Venue::Pons
    } else {
        ui::image::Venue::Uniswap
    }
}

/// The empty state: no pool selected. Named for the CHAIN, because that is the
/// one thing still true when nothing is chosen — and because a blank-looking
/// screen that is actually still pointed at last session's token is how you buy
/// a coin you never meant to touch.
fn blank_pool(net_label: &str) -> SelPool {
    SelPool {
        label: net_label.to_string(),
        kind: engine::PoolKind::V4 { pool_id: B256::ZERO, tick_spacing: 0 },
        token: alloy::primitives::Address::ZERO,
        sym: "—".into(),
        fee: 0,
        owned: false,
        quote: engine::Quote::Eth,
        quote_sym: "ETH".to_string(),
    }
}

/// Build the engine PoolCfg from a selectable pool. quote_usd starts at a safe
/// fallback and is overwritten by the live CoinGecko fetch before first render.
fn to_poolcfg(p: &SelPool) -> engine::PoolCfg {
    engine::PoolCfg {
        kind: p.kind,
        token: p.token,
        sym: p.sym.clone(),
        fee: p.fee,
        // Optimistic default; `refresh_token_decimals` reads the real value from
        // the ERC-20 as soon as the pool is active. 18 is right for almost every
        // memecoin, so this is only wrong for the brief moment before the read.
        token_decimals: 18,
        quote: p.quote,
        quote_sym: p.quote_sym.clone(),
        quote_usd: if p.quote.is_eth() { 1851.0 } else { 1.0 },
    }
}

/// Read the tracked token's `decimals()` and store it on the pool config.
///
/// Assuming 18 breaks any token that isn't (USDG is 6): reserves, balance and
/// supply all come out 10^(18-d) too small, which shows up as price 0.000000 and
/// a nonsense market cap. Called on every pool switch.
async fn refresh_token_decimals<P: Provider>(provider: &P, bot: &mut engine::Bot) {
    if bot.pool.token == alloy::primitives::Address::ZERO {
        return;
    }
    match token_metadata_chain_id::decimals(provider, bot.pool.token).await {
        Some(d) => {
            if d != bot.pool.token_decimals {
                bot.note(format!("{} uses {d} decimals", bot.pool.sym));
            }
            bot.pool.token_decimals = d;
        }
        // Non-standard tokens may omit decimals(); 18 is the sane default.
        // Not cached, so a transient read failure is retried next switch.
        None => bot.pool.token_decimals = 18,
    }
}

/// v3 orientation: WETH is token0 iff its address sorts below the token's.
fn weth_is_token0(token: alloy::primitives::Address) -> bool {
    contracts::WETH < token
}

/// Menu label with honest ownership + protocol tags: "[ours]   [v4] ETH/SYM 1%".
fn pool_label(owned: bool, proto: &str, quote_sym: &str, sym: &str, fee: u32, note: &str) -> String {
    // The quote must be in the label. It used to be hardcoded "ETH/", so a
    // USDG-quoted pool was labelled ETH/AAPL — and since the last-used pool is
    // restored BY LABEL, that label then matched a different, ETH-quoted entry
    // on the next start. Every price and reserve came out scaled by 10^12.
    format!(
        "{} [{}] {}/{} {}{}",
        if owned { "[ours]  " } else { "[public]" },
        proto,
        quote_sym,
        sym,
        fee_label(fee),
        note,
    )
}

/// The same pool, written as prose for the status line.
///
/// Menu labels carry `[ours]` / `[v3]` tags because a list needs to be scanned
/// in columns. A status line is a sentence, and in this app square brackets
/// mean "press this key" — so they must never appear in one.
fn pool_sentence(label: &str) -> String {
    let mut out = label.to_string();
    for (tag, word) in [
        ("[ours]", "your"),
        ("[public]", "public"),
        ("[v3]", "Uniswap V3"),
        ("[v4]", "Uniswap V4"),
        ("[flaunch]", "Flaunch"),
    ] {
        out = out.replace(tag, word);
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn symbol_for(net: &config::Network, addr: &str) -> String {
    net.tokens
        .iter()
        .find(|t| t.address.eq_ignore_ascii_case(addr))
        .map(|t| t.symbol.clone())
        .unwrap_or_else(|| "TOK".into())
}

/// v4 pool id = keccak256(abi.encode(PoolKey)).
fn compute_pool_id(token: alloy::primitives::Address, fee: u32, tick_spacing: i32) -> B256 {
    use alloy::sol_types::SolValue;
    let key = contracts::PoolKey {
        currency0: alloy::primitives::Address::ZERO,
        currency1: token,
        fee: fee.try_into().unwrap(),
        tickSpacing: tick_spacing.try_into().unwrap(),
        hooks: alloy::primitives::Address::ZERO,
    };
    alloy::primitives::keccak256(key.abi_encode())
}

/// Pools we own (registry) tagged [ours], then public pools tagged [public].
/// Where a chain's discovered tokens live.
///
/// Cache, not configuration. These accumulate on their own — every coin added
/// by CA or picked out of the trenches lands here — and they are reconstructible
/// from the chain at any time. Config is what a person decides; this is what the
/// app found out. Keeping them together meant a file you were told to edit grew
/// hundreds of entries you never wrote, and burying an RPC URL among them.
/// Change a setting: say it on the status line AND record it.
///
/// These decide what the next keypress spends, so a change to one is an action
/// worth keeping. It only ever reached the status line, which the next message
/// overwrites — so the size you were trading at was unrecoverable ten seconds
/// later, including from a log sent with a bug report.
fn setting(bot: &mut engine::Bot, what: &str, value: String, sentence: String) {
    events::action("Setting changed", &[("setting", what.to_string()), ("value", value)]);
    bot.status = sentence;
}

/// How long a message holds the status line before it gives way to the current
/// state. Long enough to read a confirmation, short enough that nothing sits
/// there being wrong.
const STATUS_TTL: Duration = Duration::from_secs(10);

/// The two states the status line falls back to once its news has gone stale.
const ILLIQUID: &str = "This pool has no active liquidity. Press a to add liquidity.";
const HEALTHY: &str = "Healthy — pool is live.";

/// The identifying facts of a pool, for an event line.
///
/// A label like "Uniswap V3 ETH/HOOD SpaceX FERRET 1%" names a pool to a human
/// and to nobody else. The token and the pool's own address (v3) or id (v4) are
/// what you paste into an explorer, match against a fill, or send to us.
fn pool_facts(p: &engine::PoolCfg) -> Vec<(&'static str, String)> {
    let (key, val) = match p.kind {
        engine::PoolKind::V3 { pool_addr, .. } => ("pool", format!("{pool_addr:#x}")),
        // Name it for what it is: there is no pool yet, and calling a curve
        // "pool" in a bug report sends whoever reads it to the wrong contract.
        engine::PoolKind::PonsCurve { curve, .. } => ("curve", format!("{curve:#x}")),
        engine::PoolKind::V4 { pool_id, .. }
        | engine::PoolKind::FlaunchV4 { pool_id, .. }
        | engine::PoolKind::PonsV2Pool { pool_id, .. } => ("pool_id", format!("{pool_id:#x}")),
    };
    vec![
        ("sym", p.sym.clone()),
        ("token", format!("{:#x}", p.token)),
        (key, val),
        ("proto", p.kind.proto().to_string()),
        ("fee", format!("{}bp", p.fee / 100)),
        ("quote", p.quote_sym.clone()),
    ]
}

/// Colour codes for the plain-text output before the TUI starts, or empty
/// strings where they would only be noise.
///
/// `--init` and `--version` get piped into logs and CI output as often as they
/// are read by a person. The same rule the installer follows: a terminal that
/// is not a TTY, or a NO_COLOR in the environment, means plain text.
fn ansi() -> (&'static str, &'static str, &'static str) {
    let tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let allowed = tty
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(false);
    if allowed {
        ("\u{1b}[38;5;215m", "\u{1b}[38;5;244m", "\u{1b}[0m")
    } else {
        ("", "", "")
    }
}

fn token_cache_path(network: &str) -> String {
    // Filesystem-safe: a network name comes from config and can hold anything.
    let safe: String = network
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    format!("{}/tokens-{safe}.json", state_dir())
}

/// Read a chain's cached tokens. A missing or unreadable cache is empty, never
/// an error: it can always be rebuilt by finding the coins again.
fn load_token_cache(network: &str) -> Vec<config::Token> {
    std::fs::read_to_string(token_cache_path(network))
        .ok()
        .and_then(|t| serde_json::from_str::<Vec<config::Token>>(&t).ok())
        .unwrap_or_default()
}

fn collect_pools(net: &config::Network) -> Vec<SelPool> {
    let mut v = Vec::new();
    // Config first, then cache: a token someone wrote by hand should win over
    // one the app stumbled across, and `collect_pools` dedupes downstream.
    let cached = load_token_cache(&net.name);
    for t in net.tokens.iter().chain(cached.iter()) {
        for p in &t.pools {
            if !p.is_v4() {
                continue;
            }
            let tok = match p.currency1.parse::<alloy::primitives::Address>() {
                Ok(t) => t,
                Err(_) => continue,
            };
            // Build the type-safe kind: v3 needs a pool address, v4 a pool id.
            // "flaunch" must be matched before the v4 fallback, or a cached
            // Flaunch pool would reload as plain V4 and get the wrong swap
            // builder on restart.
            let kind = if p.kind.eq_ignore_ascii_case("v3") {
                match p.address.parse::<alloy::primitives::Address>() {
                    Ok(a) if a != alloy::primitives::Address::ZERO => {
                        engine::PoolKind::V3 { pool_addr: a, weth_is_token0: weth_is_token0(tok) }
                    }
                    _ => continue,
                }
            } else if p.kind.eq_ignore_ascii_case("flaunch") {
                match p.pool_id.parse::<B256>() {
                    // Currency ordering is address ordering, so the coin's side
                    // re-derives from the token itself — no extra cached field.
                    Ok(id) if id != B256::ZERO => {
                        engine::PoolKind::FlaunchV4 { pool_id: id, coin_is_0: tok < contracts::FLETH }
                    }
                    _ => continue,
                }
            } else {
                match p.pool_id.parse::<B256>() {
                    Ok(id) if id != B256::ZERO => engine::PoolKind::V4 { pool_id: id, tick_spacing: p.tick_spacing as i32 },
                    _ => continue,
                }
            };
            let sym = symbol_for(net, &p.currency1);
            let note = p.label.find('(').map(|i| format!("  {}", &p.label[i..])).unwrap_or_default();
            let (quote, quote_sym) = quote_from(&p.currency0);
            v.push(SelPool {
                label: pool_label(p.owned, kind.proto(), &quote_sym, &sym, p.fee, &note),
                kind,
                token: tok,
                sym,
                fee: p.fee,
                owned: p.owned,
                quote,
                quote_sym,
            });
        }
    }
    for pp in &net.public_pools {
        if let Ok(tok) = pp.token.parse::<alloy::primitives::Address>() {
            let sym = if pp.sym.is_empty() { symbol_for(net, &pp.token) } else { pp.sym.clone() };
            v.push(SelPool {
                label: pool_label(false, "v4", "ETH", &sym, pp.fee, ""),
                kind: engine::PoolKind::V4 {
                    pool_id: compute_pool_id(tok, pp.fee, pp.tick_spacing as i32),
                    tick_spacing: pp.tick_spacing as i32,
                },
                token: tok,
                sym,
                fee: pp.fee,
                owned: false,
                quote: engine::Quote::Eth,
                quote_sym: "ETH".to_string(),
            });
        }
    }
    v
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    // Answered before anything touches the terminal or the registry: a bug
    // report needs the version, and a build too broken to reach the dashboard
    // is exactly when someone will be asked for it.
    if let Some(a) = std::env::args().nth(1) {
        match a.as_str() {
            "--version" | "-V" => {
                // Version AND commit: a rebuilt release carries the same tag,
                // and a bug report needs to name the build, not the label.
                println!("trenches {}", update::full());
                return Ok(());
            }
            // Create the config and state directories, then stop. The
            // installer calls this so a fresh machine has both, with the schema
            // line in place, before the app is ever opened.
            //
            // The binary does it rather than the install script, so the starter
            // config has exactly one definition. A copy in a shell script is a
            // copy that drifts the first time a field is added.
            "--init" => {
                // Always the canonical path — not whatever `config_path()`
                // resolves to from the current directory.
                let path = config::init_path();
                let created = !path.exists();
                if created {
                    if let Some(dir) = path.parent() {
                        std::fs::create_dir_all(dir)?;
                    }
                    std::fs::write(&path, config::starter_json())?;
                }
                // This is the installer's path on a fresh machine, and it
                // writes the file itself rather than going through the loader
                // — so it needs the same 0600 the rest of the config handling
                // applies. Unconditional: an existing config predating that
                // rule is exactly the one still sitting world-readable.
                config::owner_only(&path);
                std::fs::create_dir_all(state_dir())?;
                // Boxed, and coloured where colour will land.
                //
                // These two paths are the whole answer to "where did it put my
                // things", and they were four lines of prose among eight more.
                // A frame makes them the thing on the screen; `--init` is
                // usually read once, in a hurry, right after an install.
                let (amber, dim, off) = ansi();
                let head = if created { "Wrote a starter config" } else { "Config already present" };
                let cfg = path.display().to_string();
                const NOTE: &str = "   logs, cache, PnL history";
                let st = format!("{}{NOTE}", state_dir());

                // Each row's visible text, then one padding rule for all of
                // them. Measuring the pieces separately is how the box came out
                // a column short on the row with a suffix on it.
                let body = [head.to_string(), String::new(), cfg, st];
                let w = body.iter().map(|l| l.chars().count()).max().unwrap_or(0) + 4;
                let rule = "─".repeat(w);

                println!();
                println!("  {dim}┌{rule}┐{off}");
                for (i, line) in body.iter().enumerate() {
                    let pad = w - line.chars().count() - 2;
                    // The heading in accent, the trailing note dimmed; the paths
                    // themselves plain, because they are the thing being read.
                    let text = if i == 0 {
                        format!("{amber}{line}{off}")
                    } else if let Some(base) = line.strip_suffix(NOTE) {
                        format!("{base}{dim}{NOTE}{off}")
                    } else {
                        line.clone()
                    };
                    println!("  {dim}│{off}  {text}{:pad$}{dim}│{off}", "");
                }
                println!("  {dim}└{rule}┘{off}");
                println!();
                println!("  Want one key that covers every chain, websockets included?");
                println!("  Free and Pro plans: https://rpc.trenches.sh");
                println!();
                return Ok(());
            }
            // Update from the shell, for when the app is not open — a
            // long-running session, a headless box, or simply the habit of
            // updating things from a prompt.
            //
            // Synchronous and blocking, unlike the launch check: someone who
            // typed this is waiting for an answer, so it asks and reports here
            // rather than spawning a task nobody will see.
            "--update" => {
                let (amber, dim, off) = ansi();
                println!();
                println!("  {dim}Current{off}  {}", update::full());
                match update::check_now().await {
                    update::Status::Update(v) => {
                        println!("  {dim}Latest{off}   {amber}{v}{off}");
                        println!();
                        match update::install_latest().await {
                            Ok(msg) => println!("  {msg}"),
                            Err(why) => {
                                println!("  {why}");
                                println!();
                                return Ok(());
                            }
                        }
                    }
                    update::Status::Soaking { tag, ready_in } => {
                        println!("  {dim}Latest{off}   {tag}");
                        println!();
                        println!(
                            "  Published recently, so it is not offered yet — a release gets {} to",
                            update::short_hours(update::SOAK)
                        );
                        println!("  be pulled if something is wrong with it. Ready in {}.", update::short_hours(ready_in));
                        println!();
                        println!("  {dim}TRENCHES_UPDATE_NOW=1 trenches --update{off}  takes it now.");
                    }
                    update::Status::Latest => {
                        println!();
                        println!("  Already on the latest release.");
                    }
                    // Not "you are up to date": we do not know that.
                    update::Status::Unknown | update::Status::Checking => {
                        println!();
                        println!("  Could not reach GitHub to check for updates.");
                        println!("  Nothing was changed.");
                    }
                }
                println!();
                return Ok(());
            }
            "--help" | "-h" => {
                println!("trenches {}", update::full());
                println!();
                println!("A terminal for trading memecoins on Robinhood Chain and Solana.");
                println!();
                println!("USAGE:");
                println!("    trenches            start the app");
                println!("    trenches --init     write the config and state dirs, then exit");
                println!("    trenches --update   install a newer release, if there is one");
                println!("    trenches --version  print the version");
                println!();
                println!("There are no other flags — everything is a keypress once you are in.");
                println!("Press ? for the shortcuts and D for the docs.");
                println!();
                println!("Config  ~/.config/trenches/     Logs  ~/.trenches/");
                println!("Issues  https://github.com/asyncswap/trenches/issues");
                return Ok(());
            }
            _ => {}
        }
    }

    // Ask whether there is a newer release, in the background. Started here, at
    // the top, so it has answered by the time anything is drawn — and detached,
    // so a slow or absent network delays nothing. It only ever reports.
    update::spawn_check();
    // Old per-session logs pile up forever otherwise — a state dir was found
    // in the wild holding 500+ trace files. Keep the newest handful of each.
    prune_session_logs();
    // One watchdog for the whole process, not one per dashboard. A freeze in
    // the Solana app, in discovery, or in a picker used to go unrecorded
    // entirely, because the only watchdog lived inside the EVM session.
    let _ui_watchdog = spawn_ui_watchdog();
    events::info(
        "Trenches started",
        &[("version", update::full())],
    );

    // Loads what is there, or writes a starter file and says so. A first run
    // used to end on a file-not-found for a path the user had never heard of.
    let (mut reg, cfg_path, created) = Registry::load_or_create()?;

    // Testnets and local nodes are ours, not a user's.
    //
    // Empty unless the binary was built with `--features testnet`, so a release
    // does not merely hide them — a chain picker whose first entry is a testnet
    // invites a first trade that goes nowhere, and an anvil node nobody is
    // running is a dead row. Never written to the config either way.
    reg.networks.extend(config::dev_networks());

    // A build without the solana feature cannot trade Solana, so it does not
    // offer it. The dispatch below still refuses politely if one slips through,
    // but a chain in the picker that answers "not compiled in" is a door that
    // opens onto a wall — better never to draw the door.
    #[cfg(not(feature = "solana"))]
    reg.networks.retain(|n| !n.kind.is_solana());
    let _ = REGISTRY_PATH.set(cfg_path.to_string_lossy().into_owned());
    if created {
        println!();
        println!("  Welcome to Trenches.");
        println!();
        println!("  Wrote a starter config to");
        println!("    {}", cfg_path.display());
        println!();
        println!("  It works as-is on public endpoints, which are rate limited. For live");
        println!("  trading grab a key at https://rpc.trenches.sh — free at 10 req/s, Pro");
        println!("  for uncapped — or put your own provider's RPC URL in the file.");
        println!("  Never put a seed phrase in it; accounts are keystores, added with W.");
        println!();
        println!("  Starting…");
        println!();
    }

    // One native ratatui app: selection screens, then the trading dashboard.
    enable_raw_mode()?;
    std::io::stdout().execute(EnterAlternateScreen)?;
    // Mouse capture: highlighting text copies it (see ui::mouse). The
    // terminal's own selection stops working under capture, so the app
    // provides the same gesture itself.
    let _ = std::io::stdout().execute(crossterm::event::EnableMouseCapture);
    // Raw mode + alternate screen are global terminal state. A panic unwinds
    // past the teardown below and would leave the user with a shell that shows
    // no typing and no prompt, so restore it first and let the panic through.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = std::io::stdout().execute(crossterm::event::DisableMouseCapture);
        let _ = std::io::stdout().execute(LeaveAlternateScreen);
        default_hook(info);
    }));
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let res = app(&mut terminal, &reg).await;
    disable_raw_mode()?;
    let _ = std::io::stdout().execute(crossterm::event::DisableMouseCapture);
    std::io::stdout().execute(LeaveAlternateScreen)?;
    res
}

/// Solana entry point: pick an account (addresses shown in Solana's own format,
/// derived from the same registry mnemonics), then hand off to the pump.fun
/// dashboard. Keystore accounts are unlocked by password, exactly like EVM.
#[cfg(feature = "solana")]
async fn solana_app(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    reg: &Registry,
    net: &config::Network,
    // False on the way in, true when `W` sent us back round — same contract as
    // the EVM side.
    ask_account: bool,
) -> eyre::Result<Exit> {
    // Offer keystore accounts first, then mnemonic-derived ones. The addresses
    // shown are ed25519/base58 — not the EVM addresses for the same seed.
    // Same rule as the EVM side: keystores only. A seed phrase in a config file
    // is not an account we are willing to offer, so the wallet screen IS the
    // account picker here too.
    // Optional, exactly like the EVM side. Esc goes on WITHOUT an account and
    // the dashboard opens read-only: prices, launches and the tape are worth
    // seeing before committing a key to the machine, and `W` unlocks one at any
    // point. This loop only runs when `W` asked for it, so it must show even
    // with nothing to unlock: the screen offers create/import, and gating it
    // on existing keystores made `W` a silent no-op on a fresh machine.
    let mut unlocked: Option<solana_keypair::Keypair> = None;
    // `ask_account` decides WHETHER to ask at all; the loop then runs until an
    // unlock succeeds or the user backs out. It is deliberately not a
    // condition the body re-evaluates.
    if ask_account {
        loop {
            let Some(ks) = wallet_screen(terminal, config::ChainKind::Solana)? else {
                break;
            };
            let Some(pass) =
                ui::password(terminal, &format!("Password for {ks}"))?.map(Zeroizing::new)
            else {
                continue;
            };
            match sol::wallet::keypair_from_keystore(&ks, pass.as_str()) {
                Ok(kp) => {
                    save_last_wallet(&ks);
                    unlocked = Some(kp);
                    break;
                }
                Err(e) => {
                    ui::select(terminal, &format!("Could not unlock: {e}"), &["Back".into()])?;
                }
            }
        }
    }
    // No account: a throwaway key builds the client. Nothing is ever signed with
    // it — the order keys are guarded on `has_account` — but it keeps ONE code
    // path rather than a second dashboard differing only in whether it can sign.
    let has_account = unlocked.is_some();
    let signer = unlocked.unwrap_or_else(solana_keypair::Keypair::new);

    let exit = sol::app::run(
        terminal,
        net.rpc_pool(),
        net.ws_pool(),
        signer,
        has_account,
        &reg.rugcheck,
        &view::pretty_network(&net.name),
    )
    .await?;
    if exit == Exit::ChangeAccount {
        // Straight back to the wallet list on this same chain — the same shape
        // the EVM session uses, so both chains behave identically.
        return Box::pin(solana_app(terminal, reg, net, true)).await;
    }
    Ok(exit)
}

/// Wallet manager: pick a keystore, or make one.
///
/// Lists what is actually on disk rather than what the registry claims, so a
/// wallet created in `cast` shows up here and one created here shows up there.
/// Returns the chosen keystore name.
fn wallet_screen(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    kind: config::ChainKind,
) -> eyre::Result<Option<String>> {
    loop {
        let mut found = wallet::list_keystores();
        // The one you used last sits at the top: it is overwhelmingly the one
        // you want again, and hunting for it in an alphabetical list every run
        // is friction for no reason.
        if let Some(last) = load_last_wallet() {
            if let Some(i) = found.iter().position(|k| k.name == last) {
                let k = found.remove(i);
                found.insert(0, k);
            }
        }
        let mut labels: Vec<String> = vec![
            "＋ Create new account".to_string(),
            "＋ Import private key".to_string(),
            "＋ Import seed phrase".to_string(),
        ];
        // The PATH alone, not the name beside it. The keystore's filename is the
        // last segment of the path, so printing both said everything twice —
        // and what you actually check before unlocking a key is where it lives.
        labels.extend(found.iter().enumerate().map(|(i, k)| {
            let last = i == 0 && load_last_wallet().as_deref() == Some(k.name.as_str());
            format!("{}{}", short_home(&k.path), if last { "   · last used" } else { "" })
        }));

        let chain = match kind {
            config::ChainKind::Solana => "Solana",
            config::ChainKind::Evm => "EVM",
        };
        let title = if found.is_empty() {
            format!("No wallets yet — create one ({chain})")
        } else {
            format!("Select Wallet or create one ({chain})")
        };
        let Some(i) = ui::select(terminal, &title, &labels)? else {
            return Ok(None);
        };
        const ACTIONS: usize = 3;
        if i >= ACTIONS {
            return Ok(Some(found[i - ACTIONS].name.clone()));
        }

        // Creating: name, then the secret if importing, then a password.
        let Some(name) = ui::input(terminal, "Wallet name", "e.g. robin — becomes the file name")? else {
            continue;
        };
        // Zeroizing: a pasted key or seed phrase is the whole wallet, and a
        // plain String leaves it in the heap for whatever reads that page next
        // — a core dump, swap, another allocation. Wrapping at the SOURCE means
        // every path out of this loop scrubs it, including the `continue`s.
        let secret = match i {
            1 => ui::password(terminal, "Private key (hidden)")?.map(Zeroizing::new),
            2 => ui::password(terminal, "Seed phrase (hidden)")?.map(Zeroizing::new),
            _ => None,
        };
        if i > 0 && secret.is_none() {
            continue;
        }
        let Some(pass) = ui::password(terminal, "Password for the new keystore")?.map(Zeroizing::new)
        else {
            continue;
        };
        let Some(again) = ui::password(terminal, "Password again")?.map(Zeroizing::new) else {
            continue;
        };
        if pass != again {
            ui::select(terminal, "Those passwords did not match", &["Try again".into()])?;
            continue;
        }

        // Each arm returns the address as text: the two chains format addresses
        // differently, and the Solana path used to hand back a placeholder that
        // rendered as 0x000…000.
        let made: eyre::Result<String> = match i {
            1 => match kind {
                #[cfg(feature = "solana")]
                config::ChainKind::Solana => {
                    Err(eyre::eyre!("import a Solana key from its seed phrase instead"))
                }
                _ => wallet::import_private_key(
                    &name,
                    secret.as_ref().map(|s| s.as_str()).unwrap_or(""),
                    pass.as_str(),
                )
                .map(|a| a.to_string()),
            },
            2 => {
                // One phrase holds many accounts; taking index 0 silently is
                // how you import a wallet that is not the one you meant.
                let idx = ui::input(terminal, "Account index", "0 is the first account")?
                    .and_then(|t| t.trim().parse::<u32>().ok())
                    .unwrap_or(0);
                let phrase = secret.as_ref().map(|s| s.as_str()).unwrap_or("");
                // The chains derive DIFFERENTLY from the same phrase: Ethereum
                // is secp256k1 on m/44'/60', Solana is ed25519 SLIP-0010 on
                // m/44'/501'/n'/0'. Using the Ethereum path for a Solana import
                // yields a real key that is not the address Phantom shows.
                match kind {
                    #[cfg(feature = "solana")]
                    config::ChainKind::Solana => {
                        sol::wallet::create_keystore(phrase, idx, &name, pass.as_str())
                    }
                    _ => wallet::import_mnemonic(&name, phrase, idx, pass.as_str())
                        .map(|a| a.to_string()),
                }
            }
            _ => wallet::create_keystore(&name, pass.as_str()).map(|a| a.to_string()),
        };
        match made {
            Ok(addr) => {
                // Saved: no extra screen. The address is confirmed on the next
                // one, where it can be checked against the wallet you expect.
                save_last_wallet(&name);
                trace(&format!("wallet created: {name} {addr}"));
                return Ok(Some(name));
            }
            Err(e) => {
                ui::select(terminal, &format!("Could not create it: {e}"), &["Back".into()])?;
            }
        }
    }
}

/// First 6 and last 4 of an address — enough to match against an explorer
/// without eating the row.
fn short_addr(a: alloy::primitives::Address) -> String {
    let s = format!("{a:#x}");
    if s.len() <= 12 {
        return s;
    }
    format!("{}…{}", &s[..6], &s[s.len() - 4..])
}

/// `~`-shortened path, so a wallet list reads as a location rather than a wall
/// of home directory.
fn short_home(p: &std::path::Path) -> String {
    let s = p.display().to_string();
    match config::home_dir().map(|h| h.display().to_string()) {
        Some(h) if s.starts_with(&h) => s.replacen(&h, "~", 1),
        _ => s,
    }
}

/// How a chain's dashboard ended.
///
/// The chain used to be a one-way choice made at startup, so switching between
/// the EVM and Solana sides meant killing and relaunching the binary.
#[derive(PartialEq, Clone, Copy)]
pub enum Exit {
    /// Leave the program.
    Quit,
    /// Back out to the start screen — one step further than the chain picker.
    /// Esc from the chain picker lands here; nothing else uses it.
    Docs,
    /// Return to the chain picker.
    ChangeChain,
    /// Re-run account selection on the SAME chain.
    ///
    /// The signer is baked into the provider when it is built, so switching
    /// accounts means building a new provider — which is exactly what
    /// re-entering the session does. Reusing that is far less fragile than
    /// swapping a signer underneath a live provider.
    ChangeAccount,
}

async fn app(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    reg: &Registry,
) -> eyre::Result<()> {
    // Docs first.
    //
    // The first screen used to be the chain picker, which asks a question before
    // saying what any of it is. The docs are already in the binary and already
    // explain the keys, the wallets and the config — landing there costs one
    // keypress to leave and answers most of what a first run needs.
    //
    // It is also the screen Esc falls back to. Esc on the chain picker used to
    // quit the app outright, which is not what "back" means anywhere else.
    loop {
        // The docs can make an account, because the Accounts page tells you to
        // and the app is already open. `wallet_screen` handles the whole flow —
        // create, import, name, password — and its return value is a selection
        // this caller has no use for.
        //
        // Which chain has to be asked. The two derive differently from the same
        // phrase — Ethereum on m/44'/60', Solana on m/44'/501' — so guessing
        // hands someone an address that does not match what their wallet shows,
        // with nothing on screen to explain why.
        let mut make_wallet = |t: &mut Terminal<CrosstermBackend<std::io::Stdout>>| -> eyre::Result<()> {
            let opts = vec![
                "Robinhood Chain / EVM".to_string(),
                "Solana".to_string(),
            ];
            let Some(i) = ui::select(t, "An account for which chain?", &opts)? else {
                return Ok(());
            };
            let kind = if i == 1 { config::ChainKind::Solana } else { config::ChainKind::Evm };
            wallet_screen(t, kind).map(|_| ())
        };
        // Docs on the way in — the first time, or whenever the config asks for
        // them. Skipping straight to the chain picker on a machine that has
        // already been set up is the difference between onboarding and a splash
        // screen you learn to dismiss without reading.
        let show = reg.start_on_docs.unwrap_or(!config::onboarded());
        if show {
            if !ui::start_screen(terminal, &mut make_wallet)? {
                return Ok(());
            }
            config::mark_onboarded();
        }
        // Straight back to the chain used last, if there is one.
        //
        // The picker is a question with one obvious answer for anyone past
        // their first run — you trade the same chain most days. `C` still
        // changes it, and Esc from the account list lands on the picker, so
        // nothing is unreachable; it is just no longer in the way.
        let mut resume = load_last_chain()
            .and_then(|n| reg.networks.iter().position(|x| x.name == n));

        loop {
                let exit = match resume.take() {
                    Some(i) => {
                        save_last_chain(&reg.networks[i].name);
                        events::action("Resumed last chain", &[("chain", reg.networks[i].name.clone())]);
                        chain_session_on(terminal, reg, &reg.networks[i], i, false, None).await?
                    }
                    None => chain_session(terminal, reg, None).await?,
                };
                match exit {
                    Exit::ChangeChain => continue,
                    Exit::Docs => break,
                    _ => return Ok(()),
                }
            }
        }
    }

    async fn chain_session(
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
        reg: &Registry,
        keep_net: Option<usize>,
    ) -> eyre::Result<Exit> {
        // 1) Chain: just the names, mainnets first. The logo beside the list says
        // which chain it is; an id and a pool count are numbers you never pick by.
        struct Pick {
            idx: usize,
            name: String,
            mainnet: bool,
        }
        let picks: Vec<Pick> = reg
            .networks
            .iter()
            .enumerate()
            .map(|(idx, n)| {
                let env = view::network_env(&n.name);
                let base = view::pretty_network(n.name.split(['-', '_']).next().unwrap_or(&n.name));
                // "Robinhood Chain" is what it is called; "Solana" already reads as
                // a chain, so only the one that needs it gets the suffix.
                let base = if base.eq_ignore_ascii_case("robinhood") {
                    "Robinhood Chain".to_string()
                } else {
                    base
                };
                Pick {
                    idx,
                    name: if env.eq_ignore_ascii_case("mainnet") { base } else { format!("{base} ({})", env.to_lowercase()) },
                    mainnet: env.eq_ignore_ascii_case("mainnet"),
                }
            })
            .collect();

        // Wide enough that a chain name has room around it rather than filling its
        // box edge to edge. The mark beside it is square and sized off the row
        // count, so widening this does not stretch the logo.
        const COLS: &[u16] = &[44];
        // Mainnets first, then test and local. No divider row: the ordering already
        // groups them, and a rule just costs a line.
        let (mut rows, mut back): (Vec<ui::PickRow>, Vec<Option<usize>>) = (Vec::new(), Vec::new());
        for main in [true, false] {
            for p in picks.iter().filter(|p| p.mainnet == main) {
                rows.push(ui::PickRow::new([p.name.clone()]));
                back.push(Some(p.idx));
            }
        }
        // Keyed on the NETWORK, not the chain family: a local anvil node is EVM but
        // is not Robinhood, and showing their feather next to it is just wrong.
        let names: Vec<String> = reg.networks.iter().map(|n| n.name.clone()).collect();
        let brand = |row: usize| {
            back.get(row)
                .copied()
                .flatten()
                .and_then(|i| names.get(i))
                .map(|n| n.to_lowercase())
        };
        let _ = keep_net;
        // The chain picker is the first thing drawn, so it is where a waiting
        // update gets said. One line, no prompt to dismiss, no blocking.
        let title = match update::available() {
            Some(v) => format!("Select chain          ▲ {v} available — curl -fsSL https://trenches.sh/install | sh"),
            None => "Select chain".to_string(),
        };
        let chosen = ui::select_table(
            terminal,
            &title,
            &[],
            COLS,
            &rows,
            |row| brand(row).as_deref().and_then(ui::logo::for_network),
            |row| brand(row).as_deref().and_then(ui::image::for_network),
        )?;
        let (net_idx, net) = match chosen.and_then(|r| back.get(r).copied().flatten()) {
            Some(i) => {
                // Remembered only once it is actually chosen, so a chain you looked
                // at and backed out of is not where you land next time.
                save_last_chain(&reg.networks[i].name);
                events::action("Selected chain", &[("chain", reg.networks[i].name.clone())]);
                (i, &reg.networks[i])
            }
            // Esc on the first screen means back, not quit. `q` is how you leave,
            // and it asks first.
            None => return Ok(Exit::Docs),
        };
        chain_session_on(terminal, reg, net, net_idx, false, None).await
    }

    /// The session for one already-chosen network.
    async fn chain_session_on(
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
        reg: &Registry,
        net: &config::Network,
        _net_idx: usize,
        // False on the way in, true when `W` sent us back round. A first run should
        // reach the dashboard without being asked for a password it may not need —
        // watching costs nothing and unlocking is one keypress away.
        ask_account: bool,
        // The pool that was on screen before an account change, if this is one.
        // None on a fresh session.
        resume_pool: Option<SelPool>,
    ) -> eyre::Result<Exit> {

        // Solana networks take an entirely separate path: different signing curve,
        // different venues, different engine. Only the UI components are shared.
        if net.kind.is_solana() {
            #[cfg(feature = "solana")]
            {
                return solana_app(terminal, reg, net, false).await;
            }
            #[cfg(not(feature = "solana"))]
            {
                ui::select(
                    terminal,
                    "Solana support is not compiled in",
                    &["Rebuild with:  cargo build --release --features solana".to_string()],
                )?;
                return Ok(Exit::ChangeChain);
            }
        }

        // 2) Account: keystores on disk, or make one. There is no separate account
        // list — a seed phrase in a config file is not an account we are willing to
        // offer, so the only accounts are encrypted keystores.
        //
        // Optional. Esc goes on WITHOUT an account: the dashboard is worth looking
        // at before you commit a key to it — prices, launches, the tape — and
        // making an unlock the price of entry means anyone who just wants to watch
        // hands over a password first. `W` brings this up at any point — and it
        // must come up even with nothing to unlock, because the screen offers
        // create/import; gating it on existing keystores made `W` a silent no-op
        // on a machine with no ~/.foundry/keystores and no config keystores.
        let mut unlocked: Option<(String, alloy::signers::local::PrivateKeySigner)> = None;
        // As on the Solana side: whether to ask is decided once, then the loop
        // runs until an unlock lands or the user steps back.
        if ask_account {
            loop {
            let Some(ks) = wallet_screen(terminal, config::ChainKind::Evm)? else {
                break;
            };
            let Some(pass) =
                ui::password(terminal, &format!("Password for {ks}"))?.map(Zeroizing::new)
            else {
                continue;
            };
            let Some(path) = wallet::keystore_path(&ks) else {
                ui::select(terminal, &format!("{ks} is no longer on disk"), &["Back".into()])?;
                continue;
            };
            match alloy::signers::local::LocalSigner::decrypt_keystore(&path, pass.as_str()) {
                Ok(sg) => {
                    // Only after a successful unlock: a mistyped password should
                    // not change which wallet comes up next time.
                    save_last_wallet(&ks);
                    unlocked = Some((ks, sg));
                    break;
                }
                Err(_) => {
                    ui::select(terminal, "Wrong password for that wallet", &["Back".into()])?;
                }
            }
        }
    }

    // 3) Pools: ours (from the registry) + public real-world pools we added.
    let pools = collect_pools(net);
    // Start with NO pool selected, every session, deliberately.
    //
    // This used to restore the last pool used on this chain. That put a fresh
    // session one keypress away from buying whatever was open days ago — the
    // screen reads as blank-and-idle, and `b` does not care. Selecting a pool
    // is now always an explicit act.
    // Blank on the way in — restoring the pool used days ago put a fresh
    // session one keypress away from buying it, and a blank-looking screen does
    // not stop `b` from working. Changing account inside a session is the one
    // exception: you were looking at that pool a second ago, and unlocking a
    // wallet is not a request to forget it.
    let pool = resume_pool
        .clone()
        .unwrap_or_else(|| blank_pool(&view::pretty_network(&net.name)));

    // Wallet-selectable assets: ETH (currency0) + every known token on this
    // network. Used by the pair picker so the user selects assets, not addresses.
    let mut assets: Vec<(alloy::primitives::Address, String)> =
        vec![(alloy::primitives::Address::ZERO, "ETH".to_string())];
    for t in &net.tokens {
        if let Ok(a) = t.address.parse::<alloy::primitives::Address>() {
            if !assets.iter().any(|(x, _)| *x == a) {
                assets.push((a, t.symbol.clone()));
            }
        }
    }

    // No account: a throwaway key builds the provider and `trader` stays zero to
    // mark it. Nothing is ever signed with it — every key that sends is guarded
    // on that zero address — but it keeps ONE provider type and one code path,
    // rather than a second generic instantiation of the whole dashboard that
    // differs only in whether it can sign.
    let no_account = unlocked.is_none();
    let (account, signer) = match unlocked {
        Some((ks, sg)) => (ks, sg),
        None => ("(no account)".to_string(), alloy::signers::local::LocalSigner::random()),
    };
    let trader = if no_account { alloy::primitives::Address::ZERO } else { signer.address() };
    // Recorded here, where an account exists and has an address, rather than on
    // the keypress that went looking for one. The zero address is the
    // no-account placeholder, and writing it down says nothing.
    if no_account {
        events::info("Watching without an account", &[("chain", net.name.clone())]);
    } else {
        events::action(
            "Account loaded",
            &[
                ("account", format!("{trader:#x}")),
                ("keystore", account.clone()),
                ("chain", net.name.clone()),
            ],
        );
    }
    let wallet = EthereumWallet::from(signer);
    // Every request goes through the balanced transport: rotation across all
    // configured endpoints (rpc + rpcs + discovery_rpc), 429 cooldowns, and a
    // micro-cache for the head-block/gas chatter. See src/rpc.rs.
    let balanced = rpc::Balanced::new(&rpc::urls_for(net))?;
    rpc::set_shared(&balanced);
    // with_recommended_fillers() adds the gas / nonce / chain-id fillers.
    // Without it the WalletFiller tries to sign a tx that has no nonce/gas/fee
    // set → "missing properties [nonce, gas_limit, max_fee_per_gas]" on send.
    // Boxed because every `P: Provider` bound in this codebase means
    // `Provider<BoxTransport>` (the default type parameter).
    let provider = ProviderBuilder::new()
        .with_recommended_fillers()
        .wallet(wallet)
        .on_client(alloy::rpc::client::RpcClient::new(
            alloy::transports::Transport::boxed(balanced.clone()),
            false,
        ));

    // The chain the endpoint says it is must be the chain we configured.
    //
    // `with_recommended_fillers()` installs a ChainIdFiller, which takes the
    // chainId it SIGNS with from eth_chainId — the endpoint's own answer. The
    // configured chain_id was only ever used to look the network up, so an
    // endpoint pointed at (or lying about) another chain got transactions
    // signed for that chain instead, and a same-address/same-nonce approve or
    // swap can then be replayed there. Checked once, before a key can sign.
    match provider.get_chain_id().await {
        Ok(live) if live != net.chain_id => {
            eyre::bail!(
                "{} is configured as chain {} but the endpoint reports {live}. \
                 Refusing to sign: check the rpc setting for this network.",
                net.name,
                net.chain_id
            );
        }
        Ok(live) => set_chain_id(live),
        // Unreachable is the RPC layer's problem to report and retry; it is
        // not evidence of a wrong chain, so it must not block the session.
        // The configured id is what everything else in this session is already
        // working from, so key caches by it rather than by nothing.
        Err(e) => {
            trace(&format!("chain id check skipped: {e}"));
            set_chain_id(net.chain_id);
        }
    }

    // Per-session log in a .bot/ folder (created if missing).
    std::fs::create_dir_all(state_dir())?;
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let log_path = format!("{}/session-{secs}.log", state_dir());
    let log = std::fs::File::create(&log_path)?;
    set_session_log(&log_path);

    // Check the profit ledger's chain once, on the way in.
    //
    // The calendar badges the individual rows, but that screen is behind a
    // keypress and a month of scrolling — and a ledger that has been edited is
    // exactly the thing you would not think to go looking for. Say it here,
    // named, where every session start will see it.
    {
        let (fills, broken) = ledger::load_checked(&account);
        if let Some(i) = broken {
            crate::events::error(
                "A recorded trade does not match its proof — the profit ledger has been changed",
                &[
                    ("row", i.to_string()),
                    ("trade", fills.get(i).map(|f| f.sym.clone()).unwrap_or_default()),
                    ("tx", fills.get(i).map(|f| f.tx.clone()).unwrap_or_default()),
                    ("file", ledger::path(&account)),
                ],
            );
        }
    }

    let mut bot = Bot {
        trader,
        net: view::pretty_network(&net.name),
        account: account.clone(),
        pool: to_poolcfg(&pool),
        last_market_trace: None,
        chart_iv: 60, // the canonical minute candle — see the sol side's note
        arb_mode: false,
        pool_b: None,
        mkt_b: engine::Market::default(),
        sqrt_price: 0.0,
        tick: 0,
        r0: 0.0,
        r1: 0.0,
        eth: 0.0,
        token_bal: 0.0,
        ready: false,
        baseline_eth: None,
        daily_baseline: None,
        day_realized: 0.0,
        daily_day: 0,
        bought_qty: 0.0,
        bought_cost: 0.0,
        realized_pnl: 0.0,
        session_start: ledger::now(),
        last_fill_pnl: None,
        entry_mc: 0.0,
        entry_pooled_eth: 0.0,
        entry_tx: None,
        entry_at: None,
        trades: 0,
        fails: 0,
        skips: 0,
        last_side: Side::Sell,
        pending: Vec::new(),
        positions: Vec::new(),
        pos_liq: std::collections::HashMap::new(),
        mint_liq: std::collections::HashMap::new(),
        orders: engine::Bot::load_orders(trader),
        log,
        logs: VecDeque::new(),
        buy_frac: 0.05,       // buy 5% of ETH balance (fine steps)
        sell_frac: 1.00,      // sell 100% of token balance by default (10% steps)
        slippage_pct: 3.0,    // matches the previous hardcoded floor
        max_price_move: 0.0,  // impact cap OFF by default — full-size swaps / instant exits ('}' to cap, '{' lower)
        lp_frac: 0.05,        // add 5% of ETH balance as LP
        nonce: None,
        token_supply: 0.0,
        eth_usd: 1871.0,    // ETH price estimate for USD market cap (adjust as needed)
        profit_guard: false, // OFF by default — don't gate on positive EV; toggle with 'g'
        guard_dup: true,     // ON by default — stop double buys; toggle with 'n'
        min_edge_eth: 0.0,
        ref_price: 0.0,
        gas_price: 0.0,
        last_edge: 0.0,
        last_read_ms: 0.0,
        lp_permit2_done: false,
        v3_covered: false,
        own_txs: engine::Bot::load_own_txs(trader),
        drain_watch: Vec::new(),
        bought_gas: 0.0,
        gas_burned: 0.0,
        buy_step_override: None,
        acting_key: String::new(),
        ur_permit2_done: false,
        routes: routes_for(&pools, pool.token),
        socials: engine::TokenSocials::default(),
        pool_launch_block: None,
        status: "ready".into(),
    };

    let _ = log_path;
    // A position open when the last session ended is still open now, and its
    // cost basis has to come back with it — a zero basis books the next sell's
    // entire proceeds as profit, permanently, into the ledger.
    bot.restore_basis();
    // Nothing recorded for this coin? Your own trades on the saved tape can
    // still say what it cost. See `recover_basis`.
    bot.recover_basis(0);
    bot.load_daily(); // restore today's PnL baseline across restarts
    // Today's figure comes from the ledger, so it has to be read before the
    // first render — otherwise the wallet shows zero for a day that already
    // has trades in it, and disagrees with the calendar until you make another.
    bot.refresh_day_realized();
    refresh_venue_meta(&provider, &mut bot).await; // socials + pool age for the start pool

    // --- trading dashboard (same terminal), with live option switching ---
    let verified = build_verified(net);
    let mut carry: Option<SelPool> = None;
    let exit = run(terminal, &provider, &mut bot, pools, assets, net.name.clone(), net.discovery_rpc.clone(), verified, &mut carry).await?;
    if exit == Exit::ChangeAccount {
        // Straight back to the account list on this same chain. Recursing
        // rebuilds the provider around the new signer, which is the whole
        // reason this cannot be swapped in place.
        return Box::pin(chain_session_on(terminal, reg, net, _net_idx, true, carry)).await;
    }
    Ok(exit)
}

/// Remember a newly added pool, so it survives a restart.
///
/// Writes to the token CACHE, not the config file. A coin added by CA is
/// something the app learned, not something the user configured — and a config
/// file that silently grows a few hundred entries is one nobody can find their
/// own RPC URL in any more. The cache is disposable: delete it and the coins
/// come back the next time they are found.
fn persist_pool(network: &str, p: &SelPool) -> eyre::Result<()> {
    use serde_json::{json, Value};
    let path = token_cache_path(network);
    let mut tokens_val: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!([]));
    if !tokens_val.is_array() {
        tokens_val = json!([]);
    }

    let token_addr = p.token.to_string();
    let (kind, pool_id, addr, tick_spacing, currency0, state_view) = match p.kind {
        engine::PoolKind::V4 { pool_id, tick_spacing } => (
            "v4", pool_id.to_string(), String::new(), tick_spacing,
            alloy::primitives::Address::ZERO.to_string(), contracts::STATE_VIEW.to_string(),
        ),
        // currency0 records flETH so collect_pools can re-derive the coin's
        // side on reload (coin_is_0 = token < flETH).
        engine::PoolKind::FlaunchV4 { pool_id, .. } => (
            "flaunch", pool_id.to_string(), String::new(), contracts::FLAUNCH_TICK_SPACING,
            contracts::FLETH.to_string(), contracts::STATE_VIEW.to_string(),
        ),
        engine::PoolKind::V3 { pool_addr, .. } => (
            "v3", String::new(), pool_addr.to_string(), 0,
            contracts::WETH.to_string(), String::new(),
        ),
        // The curve address goes in the `address` slot, and the quote asset in
        // `currency0`, so a reload can rebuild the variant from what it reads.
        // A curve is a transient state — it graduates into a pool and this
        // entry stops being true — so it is worth re-reading the launch's phase
        // rather than trusting a saved one indefinitely.
        engine::PoolKind::PonsCurve { curve, quote } => (
            "pons_curve", String::new(), curve.to_string(), 0,
            quote.to_string(), String::new(),
        ),
        engine::PoolKind::PonsV2Pool { pool_id, quote, tick_spacing, .. } => (
            "pons_v2", pool_id.to_string(), String::new(), tick_spacing,
            quote.to_string(), contracts::STATE_VIEW.to_string(),
        ),
    };
    let pool_obj = json!({
        "label": format!("ETH/{} {}", p.sym, fee_label(p.fee)),
        "kind": kind,
        "pool_id": pool_id,
        "address": addr,
        "currency0": currency0,
        "currency1": token_addr,
        "fee": p.fee,
        "tick_spacing": tick_spacing,
        "state_view": state_view,
        "owned": p.owned,
    });

    let tokens = tokens_val.as_array_mut().unwrap();
    let existing = tokens.iter_mut().find(|t| {
        t.get("address")
            .and_then(|a| a.as_str())
            .map(|s| s.eq_ignore_ascii_case(&token_addr))
            .unwrap_or(false)
    });
    match existing {
        Some(tok) => {
            let pls = tok
                .as_object_mut()
                .unwrap()
                .entry("pools")
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .unwrap();
            let dup = pls
                .iter()
                .any(|pl| pl.get("pool_id").and_then(|x| x.as_str()) == Some(pool_id.as_str()));
            if !dup {
                pls.push(pool_obj);
            }
        }
        None => tokens.push(json!({
            "name": p.sym, "symbol": p.sym, "address": token_addr, "pools": [pool_obj]
        })),
    }
    std::fs::create_dir_all(state_dir())?;
    std::fs::write(&path, serde_json::to_string_pretty(&tokens_val)?)?;
    Ok(())
}

/// Parse the config's hand-curated verified pools into runtime form. Bad entries
/// are skipped (so a typo in the registry never crashes startup).
fn build_verified(net: &config::Network) -> Vec<discover::VerifiedPool> {
    let usdg: alloy::primitives::Address =
        "0x5fc5360D0400a0Fd4f2af552ADD042D716F1d168".parse().unwrap();
    net.verified_pools
        .iter()
        .filter_map(|v| {
            Some(discover::VerifiedPool {
                token: v.token.parse().ok()?,
                pool_id: v.pool_id.parse().ok()?,
                quote: if v.quote.eq_ignore_ascii_case("WETH") {
                    engine::Quote::Eth
                } else {
                    engine::Quote::Stable { token: usdg, decimals: 6 }
                },
                tick_spacing: v.tick_spacing,
                fee: v.fee,
                sym: v.sym.clone(),
            })
        })
        .collect()
}


#[allow(clippy::too_many_arguments)] // a swap needs every one of these; bundling them into a struct would only move the list
async fn run<P: Provider + Clone + Send + Sync + 'static>(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    provider: &P,
    bot: &mut Bot,
    mut pools: Vec<SelPool>,
    assets: Vec<(alloy::primitives::Address, String)>,
    network: String,
    discovery_rpc: Option<String>,
    verified: Vec<discover::VerifiedPool>,
    // Set to whatever pool is on screen when this returns, so an account change
    // can put it back. Unlocking a wallet is not a request to forget what you
    // were looking at.
    carry: &mut Option<SelPool>,
) -> eyre::Result<Exit> {
    use futures::StreamExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    // Header logo. Any screen that takes over clears images on entry, which
    // marks this stale, so it redraws itself on return with no bookkeeping here.
    let mut chain_logo = ui::image::Placement::default();


    use std::sync::{Arc, Mutex};

    // Resolve the STARTING pool's token decimals before anything reads the
    // market — otherwise the very first render of a non-18-dec pool (e.g. a
    // 6-dec USDG pool restored from last-pool) shows price 0.000000.
    refresh_token_decimals(provider, bot).await;
    trace_pool("startup", &bot.pool);

    // Shared state written by the background poll task, read by the UI thread.
    let market = Arc::new(Mutex::new(engine::Market::default()));
    let pool_cell = Arc::new(Mutex::new(bot.pool.as_ref()));
    let block = Arc::new(AtomicU64::new(0));
    let rpc_ok = Arc::new(std::sync::atomic::AtomicBool::new(true));
    // Live trade tape (all traders' swaps on the current pool).
    let tape: Arc<Mutex<VecDeque<engine::Swap>>> = Arc::new(Mutex::new(VecDeque::new()));
    // Arb mode: second pool ref + its market snapshot.
    let pool_b_cell: Arc<Mutex<Option<engine::PoolRef>>> = Arc::new(Mutex::new(None));
    let market_b = Arc::new(Mutex::new(engine::Market::default()));
    // Set while a full-screen modal (discovery, clusters, top tokens) owns the
    // terminal. The dashboard's numbers are not on screen then, so polling for
    // them spends the rate budget the modal's own reads need.
    let poll_paused = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Set by the R key: the next poll does a FULL read, and the tape cursor
    // re-anchors — one keypress recovers from any hiccup without a restart.
    let force_full = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let tape_relive = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Aborts its task when dropped. The poller used to be spawned and
    // forgotten, so every wallet or chain switch left the old session's
    // poller running forever — each one a full market read on its own clock.
    // Three switches in, the "every 2.8s" full read was firing every ~1s and
    // eating the rate budget three times over.
    struct AbortOnDrop(tokio::task::JoinHandle<()>);
    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    // Background polling task — the ONLY continuous RPC. Hard 1.5s timeouts so a
    // slow/broken RPC can never freeze the UI; the render loop keeps running.
    let _poller = {
        let provider = provider.clone();
        let market = market.clone();
        let pool_cell = pool_cell.clone();
        let block = block.clone();
        let rpc_ok = rpc_ok.clone();
        let tape = tape.clone();
        let pool_b_cell = pool_b_cell.clone();
        let market_b = market_b.clone();
        let poll_paused = poll_paused.clone();
        let force_full = force_full.clone();
        let tape_relive = tape_relive.clone();
        let trader = bot.trader;
        AbortOnDrop(tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(350));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut last_swap_block = 0u64; // last block scanned for the tape (pool A)
            let mut last_swap_block_b = 0u64; // pool B (arb mode) tape cursor
            let mut tape_pref = *pool_cell.lock().unwrap();
            // Reopening a coin starts from its saved tape, not an empty room.
            *tape.lock().unwrap() = load_evm_tape(&tape_pref.token);
            let mut tape_b_key = alloy::primitives::Address::ZERO;
            // Balances, gas price and total supply are read once a second; the
            // price is read every poll. Reading all four every time was ~30
            // requests a second at this cadence, three quarters of it re-asking
            // for a supply that cannot change and a balance that changes only
            // when you trade — and it earned a 429 on the public endpoint, which
            // then delayed the one read that does have to be current.
            let mut poll: u64 = 0;
            let mut full_pref_token = alloy::primitives::Address::ZERO;
            let mut had_full = false;
            loop {
                tick.tick().await;
                if poll_paused.load(Ordering::Relaxed) {
                    continue;
                }
                let pref = *pool_cell.lock().unwrap();
                poll += 1;
                // Full on schedule — and IMMEDIATELY on a pool switch. Waiting
                // for the next scheduled full read left the new pool's balance
                // and supply empty for up to 2.8 seconds after selecting it,
                // which read as the app being slow rather than the poll being
                // scheduled.
                let switched = pref.token != full_pref_token || !had_full;
                let full = poll % 8 == 1 || switched || force_full.swap(false, Ordering::Relaxed);
                if full {
                    full_pref_token = pref.token;
                    had_full = true;
                }
                match tokio::time::timeout(
                    Duration::from_millis(1500),
                    async {
                        if full {
                            engine::read_market(&provider, pref, trader).await
                        } else {
                            engine::read_price_only(&provider, pref, trader).await
                        }
                    },
                )
                .await
                {
                    Ok(Ok(m)) => { *market.lock().unwrap() = m; rpc_ok.store(true, Ordering::Relaxed); }
                    _ => { rpc_ok.store(false, Ordering::Relaxed); }
                }
                // Arb mode: also read the second pool.
                let pref_b = *pool_b_cell.lock().unwrap();
                if let Some(pb) = pref_b {
                    // The second pool gets the same treatment as the first:
                    // reading it in full every poll would have doubled the
                    // request rate the moment arb mode was switched on.
                    let read_b = async {
                        if full {
                            engine::read_market(&provider, pb, trader).await
                        } else {
                            engine::read_price_only(&provider, pb, trader).await
                        }
                    };
                    if let Ok(Ok(mb)) = tokio::time::timeout(Duration::from_millis(1500), read_b).await {
                        *market_b.lock().unwrap() = mb;
                    }
                }
                if let Ok(Ok(b)) =
                    tokio::time::timeout(Duration::from_millis(1500), provider.get_block_number()).await
                {
                    block.store(b, Ordering::Relaxed);
                    // Pool switched? reset the tape scan window.
                    if pref.token != tape_pref.token {
                        // Bank the coin we are leaving, seed the one we enter.
                        save_evm_tape(&tape_pref.token, &tape.lock().unwrap());
                        *tape.lock().unwrap() = load_evm_tape(&pref.token);
                        last_swap_block = 0;
                        tape_pref = pref;
                    }
                    // R pressed? re-anchor both tape cursors — the rows stay,
                    // the next read starts from a window that can succeed.
                    if tape_relive.swap(false, Ordering::Relaxed) { last_swap_block = 0; last_swap_block_b = 0; }
                    // Scan a recent window for new swaps on this pool (cap range).
                    // Never ask for more blocks than the endpoint will serve.
                    //
                    // This chain's free tier caps eth_getLogs at TEN blocks and
                    // answers anything wider with a 400. That turned one missed
                    // scan into a permanent one: `from` only advances on
                    // success, so a failure widened the window, which
                    // guaranteed the next failure. The tape sat empty while the
                    // chart moved, and the request was never even served.
                    //
                    // Clamping to the live edge is the right trade anyway — a
                    // tape is for what is happening now, and re-anchoring beats
                    // replaying history nobody is watching.
                    // A FRESH pool asks for real history, in slices the
                    // endpoint will serve; a live tape asks only for what is
                    // new. Without the backfill a quiet coin shows an empty
                    // tape forever — correct, and useless.
                    let seeding = last_swap_block == 0;
                    let from = if seeding { b.saturating_sub(200) } else { last_swap_block + 1 };
                    if b >= from {
                        // Only advance the scan cursor when the fetch SUCCEEDS —
                        // otherwise a timeout/error would skip those blocks' events.
                        // 20 slices covers the 200-block seed; a live window
                        // needs one or two. Longer budget while seeding,
                        // because it is once per pool and worth waiting for.
                        let (calls, budget) = if seeding { (20, 6_000) } else { (3, 1_500) };
                        let scan = tokio::time::timeout(
                            Duration::from_millis(budget),
                            engine::read_swaps_chunked(&provider, pref, from, b, calls),
                        )
                        .await;
                        // Say what the scan DID. An empty tape next to a moving
                        // chart has three possible causes — the window, the
                        // fetch, the decode — and no way to tell them apart
                        // from the outside.
                        match &scan {
                            Ok(Ok(v)) => trace(&format!("tape: {}..{} -> {} swap(s)", from, b, v.len())),
                            Ok(Err(e)) => trace(&format!("tape: {}..{} FAILED {e}", from, b)),
                            Err(_) => trace(&format!("tape: {}..{} timed out after 1500ms", from, b)),
                        }
                        if let Ok(Ok(sw)) = scan {
                            // The head `b` and these logs can come from DIFFERENT
                            // endpoints, and their views of the chain differ by
                            // ~10 blocks here. Trusting `b` skipped the blocks
                            // the slower endpoint had not indexed yet — trades
                            // lost silently, forever: pool numbers moved while
                            // the tape showed nothing. Advance only to what was
                            // OBSERVED, or head-minus-a-guard when quiet, and
                            // let the overlap dedup below absorb the re-asks.
                            let observed = sw.iter().map(|s| s.block).max().unwrap_or(0);
                            let mut t = tape.lock().unwrap();
                            let have: std::collections::HashSet<_> =
                                t.iter().map(|s| (s.tx, s.block, s.eth_wei)).collect();
                            let mut added = false;
                            for s in sw {
                                // A confirmed order is injected as a PLACEHOLDER
                                // before its log arrives, so your own fill shows
                                // up even when a throttled endpoint answers the
                                // window thinly. That row carries eth_wei == 0
                                // and a price reconstructed from order-time
                                // facts — which is why it never matched this
                                // dedup key, and one swap rendered as two rows
                                // with different prices and pooled depth.
                                //
                                // The real log is authoritative, so retire the
                                // placeholder rather than sitting beside it.
                                // Matched on tx AND the zero marker, so a
                                // genuine multi-hop (two real logs, one tx)
                                // still keeps both legs.
                                if s.eth_wei != 0 {
                                    t.retain(|x| !(x.tx == s.tx && x.eth_wei == 0));
                                }
                                if !have.contains(&(s.tx, s.block, s.eth_wei)) {
                                    t.push_back(s);
                                    added = true;
                                }
                            }
                            while t.len() > 400 { t.pop_front(); }
                            if added {
                                save_evm_tape(&tape_pref.token, &t);
                            }
                            drop(t);
                            last_swap_block = observed.max(b.saturating_sub(5)).max(last_swap_block);
                        } else if last_swap_block == 0 || b.saturating_sub(from) > 40 {
                            // Wide ranges are only served by the rate-limited
                            // public endpoint. The FIRST window (200 blocks of
                            // history) needs it — and so does a cursor that
                            // fell behind while that endpoint rested, whose
                            // catch-up range then GREW every failed tick: the
                            // tape froze minutes in the past while new trades
                            // rolled on. Once the gap passes ~4s of chain,
                            // give up the backfill and re-anchor to LIVE —
                            // the next narrow read succeeds on any endpoint.
                            last_swap_block = b.saturating_sub(9);
                        }
                    }
                    // Arb mode: merge the SECOND pool's swaps into the same tape
                    // (matches gmgn's token-aggregate view across venues).
                    if let Some(pb) = pref_b {
                        if pb.token != tape_b_key { last_swap_block_b = 0; tape_b_key = pb.token; }
                        let seeding_b = last_swap_block_b == 0;
                        let from_b = if seeding_b { b.saturating_sub(200) } else { last_swap_block_b + 1 };
                        if b >= from_b {
                            let (calls_b, budget_b) = if seeding_b { (20, 6_000) } else { (3, 1_500) };
                            if let Ok(Ok(sw)) = tokio::time::timeout(
                                Duration::from_millis(budget_b),
                                engine::read_swaps_chunked(&provider, pb, from_b, b, calls_b),
                            )
                            .await
                            {
                                // Same observed-block rule + dedup as pool A.
                                let observed = sw.iter().map(|s| s.block).max().unwrap_or(0);
                                let mut t = tape.lock().unwrap();
                                let have: std::collections::HashSet<_> =
                                    t.iter().map(|s| (s.tx, s.block, s.eth_wei)).collect();
                                for s in sw {
                                    // Same placeholder retirement as pool A.
                                    if s.eth_wei != 0 {
                                        t.retain(|x| !(x.tx == s.tx && x.eth_wei == 0));
                                    }
                                    if !have.contains(&(s.tx, s.block, s.eth_wei)) {
                                        t.push_back(s);
                                    }
                                }
                                while t.len() > 400 { t.pop_front(); }
                                drop(t);
                                last_swap_block_b = observed.max(b.saturating_sub(5)).max(last_swap_block_b);
                            } else if last_swap_block_b == 0 || b.saturating_sub(from_b) > 40 {
                                // Same re-anchor as pool A: live beats backfill.
                                last_swap_block_b = b.saturating_sub(9);
                            }
                        }
                    } else {
                        last_swap_block_b = 0;
                    }
                }
            }
        }))
    };

    let mut prices: VecDeque<f64> = VecDeque::new();
    let mut tele_ctr: u64 = 0;
    // For ageing the status line out; see the act tick.
    let mut last_status = String::new();
    let mut status_since = std::time::Instant::now();
    let mut view = Panel::Tape; // default to the live tape
    let mut show_help = false;
    let mut orders_scroll: usize = 0;
    // Whether the orders panel shows every order or only the open token's.
    //
    // Defaults to the open token: when you are looking at a coin, "have I
    // traded this before, and at what?" is the question in front of you, and
    // the answer was previously buried among every other coin's orders. Press
    // `o` again for all of them.
    let mut orders_all = false;
    let mut reader = crossterm::event::EventStream::new();
    // Highlight-to-copy (see ui::mouse): drag paints, release copies. The
    // text is read off the NEXT rendered frame, where the full buffer is in
    // hand and the inversion has already been dropped.
    let mut msel = ui::mouse::Selection::default();
    let mut copy_armed = false;
    // Live USD feed (CoinGecko) for quote currencies. Fetched up front so ETH
    // and any stablecoin quotes are valued correctly before the first render —
    // but BOUNDED: this await is on the UI thread, and CoinGecko being slow
    // used to hold the whole dashboard black for up to its 6s timeout. Past
    // 1.5s the fetch moves to the background and lands via `feed_cell`.
    let feed_ids = price_ids(&pools);
    let feed_cell: Arc<Mutex<Option<std::collections::HashMap<String, f64>>>> = Default::default();
    let mut feed = match tokio::time::timeout(Duration::from_millis(1500), pricing::fetch_usd(&feed_ids)).await {
        Ok(f) => f,
        Err(_) => {
            let (ids, cell) = (feed_ids.clone(), feed_cell.clone());
            tokio::spawn(async move {
                let f = pricing::fetch_usd(&ids).await;
                if !f.is_empty() {
                    *cell.lock().unwrap() = Some(f);
                }
            });
            Default::default()
        }
    };
    apply_prices(bot, &feed);
    let mut price_refresh: u32 = 0; // action-tick counter; refetch every ~60s

    // The freeze detector. The render arm beats this heart every 100ms; a
    // watcher task reports any gap — because EVERY await in the select arms
    // below runs on the UI thread, and a slow one freezes rendering, the
    // block counter, everything. "The app froze and came back" was
    // undiagnosable without a line saying how long it froze and what held it.
    ui_alive();
    // The watchdog is process-wide now (spawned in main), so this loop only
    // has to say what it is doing.
    macro_rules! phase {
        ($p:expr) => {
            crate::ui_phase_set(::std::convert::AsRef::<str>::as_ref(&$p));
        };
    }

    // Render on a fast fixed cadence — never does RPC, so it stays smooth even
    // when the node is slow. Actions run on their own slower tick.
    let mut render = tokio::time::interval(Duration::from_millis(100));
    render.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut act = tokio::time::interval(Duration::from_millis(500));
    act.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            // fast render — pulls the latest snapshot, no network I/O
            _ = render.tick() => {
                ui_alive();
                phase!("idle");
                let m = *market.lock().unwrap();
                bot.apply_market(m);
                // Same rule as pool A: a light read has no reserves to give, so
                // take only what it actually fetched or the panel blinks.
                let mb = *market_b.lock().unwrap();
                if mb.full {
                    bot.mkt_b = mb;
                } else {
                    bot.mkt_b.sqrt_price = mb.sqrt_price;
                    bot.mkt_b.tick = mb.tick;
                    bot.mkt_b.read_ms = mb.read_ms;
                }
                let blk = block.load(Ordering::Relaxed);
                // Reference for the profitability filter: a 60-sample SMA.
                bot.ref_price = if prices.is_empty() { bot.price() } else { prices.iter().sum::<f64>() / prices.len() as f64 };
                if !rpc_ok.load(Ordering::Relaxed) {
                    bot.status = "Cannot reach the RPC endpoint. Retrying now.".into();
                }
                let mut tape_snap: Vec<engine::Swap> = tape.lock().unwrap().iter().copied().collect();
                tape_snap.sort_by_key(|s| s.block); // merged pools -> chronological
                let mut logo_box = None;
                let mut grabbed: Option<String> = None;
                // Named so a freeze here is attributable: a draw blocks when
                // the TERMINAL stops consuming output — scrollback, a dragged
                // window, a busy tab — not because of anything in the app.
                phase!("drawing (a stalled draw usually means the terminal itself was busy)");
                terminal.draw(|f| {
                    logo_box = draw(f, bot, blk, bot.last_read_ms, view, orders_scroll, orders_all, &tape_snap, show_help);
                    ui::mouse::paint(f, &msel);
                    if copy_armed {
                        if let Some((a, b)) = msel.region() {
                            grabbed = Some(ui::mouse::selected_text(f.buffer_mut(), a, b));
                        }
                    }
                })?;
                phase!("idle");
                if let Some(t) = grabbed {
                    copy_armed = false;
                    msel.clear();
                    if !t.is_empty() {
                        ui::mouse::copy(&t);
                        bot.status = format!("copied {} characters", t.chars().count());
                    }
                }
                // After the frame, so ratatui's own output cannot cover it. The
                // placement only redraws when its key changes, so adjusting a
                // value does not make it blink.
                let venue = header_venue(bot);
                let term_size = terminal.size().map(|s| (s.width, s.height)).unwrap_or((0, 0));
                if let (Some(r), Some(png)) = (logo_box, ui::image::for_venue(venue, &bot.net)) {
                    chain_logo.show(png, venue as usize, r.x, r.y, r.width, r.height, term_size);
                }
            }
            // slower action tick — auto-strategy + reap, each timeout-bounded
            _ = act.tick() => {
                if bot.ready {
                    // The condition that wrote this has passed, so the sentence
                    // has to go with it. It was set once and never cleared, so a
                    // pool that filled up seconds later still read as empty —
                    // under a tape of live trades, which is worse than saying
                    // nothing at all.
                    if bot.status == ILLIQUID {
                        bot.status = HEALTHY.into();
                    }
                    prices.push_back(bot.price());
                    if prices.len() > 60 { prices.pop_front(); }
                } else if rpc_ok.load(Ordering::Relaxed) && !bot.pool.kind.is_empty() {
                    // Only when a pool is actually selected. `ready` is false
                    // both for a pool with no liquidity AND for no pool at all,
                    // so the empty state was being told to add liquidity to a
                    // pool it did not have — and the line overwrote whatever the
                    // last action had said.
                    bot.status = ILLIQUID.into();
                }
                // Everything on this line was true when it was written; most of
                // it stops being true. Anything that has sat unchanged for
                // STATUS_TTL gives way to the state of things, so the line
                // answers "how are we now" rather than "what happened once".
                if bot.status != last_status {
                    last_status = bot.status.clone();
                    status_since = std::time::Instant::now();
                } else if status_since.elapsed() > STATUS_TTL
                    && !bot.status.is_empty()
                    && bot.status != HEALTHY
                {
                    bot.status = if !rpc_ok.load(Ordering::Relaxed) {
                        "Waiting on the RPC.".into()
                    } else if bot.pool.kind.is_empty() {
                        String::new()
                    } else if bot.ready {
                        HEALTHY.into()
                    } else {
                        ILLIQUID.into()
                    };
                    last_status = bot.status.clone();
                    status_since = std::time::Instant::now();
                }
                phase!("settling pending orders");
                let _ = tokio::time::timeout(Duration::from_secs(3), bot.reap(provider)).await;
                // Did a coin we just bought take the balance back? Only fires
                // when a buy is due a re-check, so it costs nothing otherwise.
                phase!("checking a recent buy for a drain");
                let _ = tokio::time::timeout(Duration::from_secs(2), bot.check_drains(provider)).await;
                // A pons v2 launch can graduate mid-session, at which point the
                // curve stops accepting trades entirely. Costs one call, and
                // only while a curve is open.
                phase!("checking whether a curve has graduated");
                let _ = tokio::time::timeout(Duration::from_secs(2), bot.check_graduation(provider)).await;
                // YOUR confirmed trades are guaranteed a tape row. The tape is
                // built from getLogs, and this chain's public endpoint can
                // answer a window thinly while rate limited — when that window
                // held your buy, the tape showed everyone's trades but yours.
                // Whatever the logs said, a confirmed order of the current
                // pool that the tape lacks is injected from what the order
                // already knows.
                //
                // Not until the head block is known, though. Age on the tape is
                // block distance, so a row injected at block 0 is not merely
                // undated — it is dated 28 days ago and sorts to the top, which
                // is how a swap made 22 hours ago read as a month old. There is
                // nothing to lose by waiting: the next poll injects it.
                if block.load(Ordering::Relaxed) > 0 {
                    let mut t = tape.lock().unwrap();
                    let have: std::collections::HashSet<_> = t.iter().map(|s| s.tx).collect();
                    for o in bot.orders.iter() {
                        let (Some(h), engine::OrderStatus::Confirmed) = (o.hash, o.status) else { continue };
                        if o.token != bot.pool.token || have.contains(&h) {
                            continue;
                        }
                        let action = if o.label.starts_with("BUY") {
                            engine::TapeAction::Buy
                        } else if o.label.starts_with("SELL") {
                            engine::TapeAction::Sell
                        } else {
                            continue;
                        };
                        // Reconstructed from order-time facts: price from the
                        // recorded market cap, depth from recorded pooled ETH.
                        let price = if o.mc > 0.0 { bot.token_supply / o.mc } else { 0.0 };
                        t.push_back(engine::Swap {
                            action,
                            eth: o.eth,
                            eth_wei: 0,
                            price,
                            liq_eth: o.pooled,
                            // The block the trade LANDED in, not the one we
                            // happen to be on. Stamping "now" is what made an
                            // hour-old fill reappear at the top of the tape
                            // dated seconds ago every time you came back to the
                            // token. For orders recorded before the receipt's
                            // block was kept, walk back from the wall clock at
                            // this chain's ~10 blocks/sec — an estimate, but one
                            // that puts the row within a minute of the truth
                            // instead of an hour out.
                            block: if o.block > 0 {
                                o.block
                            } else if o.at > 0 {
                                let now = block.load(Ordering::Relaxed);
                                let ago = crate::ledger::now().saturating_sub(o.at);
                                now.saturating_sub(ago.saturating_mul(10))
                            } else {
                                block.load(Ordering::Relaxed)
                            },
                            tx: h,
                            trader: bot.trader,
                            tick_lo: 0,
                            tick_hi: 0,
                            is_v4: o.is_v4,
                        });
                    }
                }
                phase!("telemetry");
                tele_ctr += 1;
                if tele_ctr.is_multiple_of(4) { bot.telemetry(block.load(Ordering::Relaxed), bot.last_read_ms); }
                // Refresh the USD feed every ~60s (120 * 500ms) so quote values
                // track — in the BACKGROUND. This await used to sit on the UI
                // thread, and a slow CoinGecko froze the whole screen for up to
                // its 6s timeout, once a minute: exactly the "it freezes and
                // then comes back" that was impossible to attribute.
                price_refresh += 1;
                if price_refresh >= 120 {
                    price_refresh = 0;
                    let (ids, cell) = (feed_ids.clone(), feed_cell.clone());
                    tokio::spawn(async move {
                        let f = pricing::fetch_usd(&ids).await;
                        if !f.is_empty() {
                            *cell.lock().unwrap() = Some(f);
                        }
                    });
                }
                if let Some(f) = feed_cell.lock().unwrap().take() {
                    feed = f;
                    apply_prices(bot, &feed);
                }
                phase!("idle");
            }
            // key input — handled the instant it arrives
            ev = reader.next() => {
                if let Some(Ok(Event::Mouse(m))) = ev {
                    use crossterm::event::MouseEventKind as MK;
                    match m.kind {
                        // The wheel scrolls whatever panel is showing, three
                        // rows per notch — the arrows' one-at-a-time is for
                        // precision, not for covering a 143-row tape.
                        MK::ScrollUp => {
                            let n = match view { Panel::Tape => tape.lock().unwrap().len(), Panel::Logs => bot.logs.len(),
                                // Orders may be filtered to the open token; scrolling
                                // past the end of what is drawn does nothing but feel broken.
                                _ => if !orders_all && !bot.pool.token.is_zero() {
                                    bot.orders.iter().filter(|o| o.token == bot.pool.token).count()
                                } else { bot.orders.len() } };
                            orders_scroll = (orders_scroll + 3).min(n.saturating_sub(1));
                        }
                        MK::ScrollDown => orders_scroll = orders_scroll.saturating_sub(3),
                        _ => {
                            if msel.on_mouse(m) {
                                copy_armed = true; // extracted on the next frame
                            }
                        }
                    }
                }
                if let Some(Ok(Event::Key(k))) = ev {

                    if k.kind != crossterm::event::KeyEventKind::Press { continue; }
                    // Any await a key arm does holds the UI; name the key so a
                    // freeze report says which action was responsible.
                    phase!(format!("the {:?} key's action", k.code));
                    // Help overlay: '?' opens it; any other key closes it.
                    if show_help {
                        show_help = false;
                        if k.code == KeyCode::Char('?') { continue; }
                    }
                    // No account: nothing can be signed, and the throwaway key
                    // that built the provider must never be asked to try.
                    if bot.trader.is_zero()
                        && matches!(k.code, KeyCode::Char('b' | 's' | 'a' | 'r' | 'x' | 'S'))
                    {
                        bot.status = "no account — press [W] to unlock one".into();
                        continue;
                    }
                    // Nothing selected means nothing to trade: the order keys
                    // have no pool to act on, and the zero address would go to
                    // the router as if it were a token. Say so instead.
                    if bot.pool.token.is_zero()
                        && matches!(k.code, KeyCode::Char('b' | 's' | 'a' | 'r' | 'x' | 'S' | 'c'))
                    {
                        bot.status = "no pool selected — press [f] to find pools".into();
                        continue;
                    }
                    // The key that caused whatever follows, stamped HERE — once,
                    // for every key — rather than inside the arms that trade.
                    //
                    // It used to be set in four arms and cleared in none, which
                    // does not mean the others recorded nothing: they recorded
                    // the last key that DID remember. Every sell in the file
                    // came out labelled `b`, because a buy had set it and
                    // nothing ever unset it. A wrong answer in this column is
                    // worse than no answer, because the column exists to be
                    // believed.
                    //
                    // So: every key sets it, and every key that cannot trade
                    // clears it. An arm added later that forgets now records
                    // "—", which is true, instead of inheriting a lie.
                    bot.acting_key = match k.code {
                        KeyCode::Char(c @ ('b' | 's' | 'x' | 'a' | 'r' | 'S' | 'e' | 'h')) => {
                            c.to_string()
                        }
                        _ => String::new(),
                    };
                    match k.code {
                        // `q` is one keystroke away from every other action, so
                        // it asks first. `Q` is the deliberate escape hatch.
                        KeyCode::Char('q') => {
                            if ui::confirm(terminal, "Quit?")? {
                                return Ok(Exit::Quit);
                            }
                        }
                        KeyCode::Char('Q') => {
                            events::action(
                                "Quit",
                                &[
                                    ("chain", bot.net.clone()),
                                    ("trades", bot.trades.to_string()),
                                    ("fails", bot.fails.to_string()),
                                    ("session_pnl", format!("{:+.6} {}", bot.pnl(), bot.pool.quote_sym)),
                                ],
                            );
                            return Ok(Exit::Quit);
                        }
                        // Update, on purpose and never otherwise. The check runs
                        // at launch and only reports; this is the one path that
                        // installs anything, and it asks first.
                        KeyCode::Char('U') => match update::status() {
                            // Each of these is a different fact. Saying
                            // "latest" for all three is what told someone on
                            // 0.1.2 they were current while 0.1.3 was out.
                            update::Status::Checking => {
                                bot.note("Still checking for updates…".to_string())
                            }
                            update::Status::Unknown => bot.note(
                                "Could not reach GitHub to check for updates.".to_string(),
                            ),
                            update::Status::Soaking { tag, ready_in } => bot.note(format!(
                                "{tag} was published recently. It will be offered in {} — set TRENCHES_UPDATE_NOW=1 to take it now.",
                                update::short_hours(ready_in)
                            )),
                            update::Status::Latest => bot.note(format!(
                                "You are on the latest version ({}).",
                                update::full()
                            )),
                            update::Status::Update(v) => {
                                if ui::confirm(terminal, &format!("Update to {v}?"))? {
                                    events::action("Updating", &[("to", v.clone())]);
                                    bot.note(format!("Installing {v}…"));
                                    match update::install_latest().await {
                                        Ok(msg) => {
                                            events::action("Update installed", &[("version", v)]);
                                            bot.note(msg);
                                        }
                                        Err(why) => {
                                            events::error("Update failed", &[("reason", why.clone())]);
                                            bot.note(why);
                                        }
                                    }
                                }
                            }
                        },
                        KeyCode::Char('D') => {
                            events::action("Opened docs", &[]);
                            ui::docs(terminal)?
                        }
                        // Ask the copilot about the room — same v1 as the
                        // Solana side: the user's own claude binary, fed the
                        // live state. Reads everything, trades nothing.
                        #[cfg(feature = "agent")]
                        KeyCode::Char('A') => {
                            if let Some(q) =
                                ui::input(terminal, "Ask the copilot", "e.g. what does this tape say?")?
                            {
                                if !q.trim().is_empty() {
                                    let mut ctx = String::new();
                                    ctx.push_str(&format!(
                                        "Network: {} (EVM)\nPool: {} / {} \n",
                                        bot.net, bot.pool.sym, bot.pool.quote_sym
                                    ));
                                    if bot.eth_usd > 0.0 {
                                        ctx.push_str(&format!("ETH/USD: {:.2}\n", bot.eth_usd));
                                    }
                                    ctx.push_str(&format!(
                                        "My ETH: {:.6}. My {}: {:.4}. Basis {:.6} {}/{}.\n",
                                        bot.eth,
                                        bot.pool.sym,
                                        bot.token_bal,
                                        bot.avg_basis(),
                                        bot.pool.quote_sym,
                                        bot.pool.sym
                                    ));
                                    ctx.push_str(&format!(
                                        "Realized PnL: {:+.6} {}. Open liquidity {:.4} ETH across {} positions.\n",
                                        bot.realized_pnl,
                                        bot.pool.quote_sym,
                                        bot.our_liq_eth(),
                                        bot.positions.len()
                                    ));
                                    {
                                        let t = tape.lock().unwrap();
                                        ctx.push_str("Recent tape, newest first (ETH amounts; MINE marks my fills):\n");
                                        for s in t.iter().rev().take(40) {
                                            let kind = match s.action {
                                                engine::TapeAction::Buy => "BUY",
                                                engine::TapeAction::Sell => "SELL",
                                                engine::TapeAction::Add => "ADD-LP",
                                                engine::TapeAction::Remove => "REMOVE-LP",
                                            };
                                            let mine = bot.own_txs.contains(&s.tx);
                                            ctx.push_str(&format!(
                                                "  {kind} {:.4} ETH pooled {:.2} ETH{}\n",
                                                s.eth,
                                                s.liq_eth,
                                                if mine { "  MINE" } else { "" }
                                            ));
                                        }
                                    }
                                    ctx.push_str(&format!("Status line: {}\n", bot.status));
                                    let rx = agent::spawn_ask(ctx, q.clone());
                                    if let Some(ans) = ui::wait_for_answer(terminal, &q, &rx)? {
                                        ui::text_view(terminal, " Copilot ", &ans)?;
                                    }
                                }
                            }
                        }
                        // Back to the account list on this same chain.
                        KeyCode::Char('W') => {
                            // Hand the current pool back so the rebuilt session
                            // can restore it. Matched by token: `pools` holds
                            // the selectable form, `bot.pool` the live one.
                            *carry = pools
                                .iter()
                                .find(|q| q.token == bot.pool.token && !bot.pool.kind.is_empty())
                                .cloned();
                            // No "from" when there is nothing loaded — the zero
                            // address is a placeholder, not a wallet.
                            let from =
                                if bot.trader.is_zero() { String::new() } else { format!("{:#x}", bot.trader) };
                            events::action("Opened the account picker", &[("current", from)]);
                            return Ok(Exit::ChangeAccount);
                        }
                        // Back to the chain picker without restarting.
                        KeyCode::Char('C') => {
                            events::action("Changing chain", &[("from", bot.net.clone())]);
                            return Ok(Exit::ChangeChain);
                        }
                        KeyCode::Char('?') => { show_help = true; }
                        // Theme picker with live preview (persists the choice).
                        KeyCode::Char('T') => {
                            match ui::widgets::theme_picker(terminal)? {
                                Some(name) => {
                                    events::action("Changed theme", &[("theme", name.clone())]);
                                    bot.status = format!("Changed theme to {name}");
                                }
                                None => bot.status = "theme unchanged".into(),
                            }
                        }
                        // View cycling: 'l' or → next, ← previous. Reset scroll.
                        // Direct panel keys — a panel is a destination, not a
                        // stop on a carousel: t trades, o orders, l logs,
                        // v chart. The arrows still cycle for the habit.
                        KeyCode::Char('t') => { view = Panel::Tape; orders_scroll = 0; }
                        // First `o` opens the panel; pressing it again widens
                        // from this token's orders to every order.
                        KeyCode::Char('o') => {
                            if view == Panel::Orders { orders_all = !orders_all; }
                            view = Panel::Orders;
                            orders_scroll = 0;
                        }
                        KeyCode::Char('l') => { view = Panel::Logs; orders_scroll = 0; }
                        // Capital O spins the carousel for one-handed browsing;
                        // the lowercase keys stay the fast direct jumps.
                        KeyCode::Char('O') | KeyCode::Right => { view = match view { Panel::Orders => Panel::Tape, Panel::Tape => Panel::Chart, Panel::Chart => Panel::Logs, Panel::Logs => Panel::Orders }; orders_scroll = 0; }
                        KeyCode::Left => { view = match view { Panel::Orders => Panel::Logs, Panel::Logs => Panel::Chart, Panel::Chart => Panel::Tape, Panel::Tape => Panel::Orders }; orders_scroll = 0; }
                        // Straight to the chart; , . walk the candle interval.
                        KeyCode::Char('c') | KeyCode::Char('v') => { view = Panel::Chart; orders_scroll = 0; }
                        KeyCode::Char(',') => { bot.chart_iv = view::iv_step(bot.chart_iv, false); bot.status = format!("candles: {}", view::iv_label(bot.chart_iv)); }
                        KeyCode::Char('.') => { bot.chart_iv = view::iv_step(bot.chart_iv, true); bot.status = format!("candles: {}", view::iv_label(bot.chart_iv)); }
                        // Scroll the active panel (↑ older, ↓ newer) — orders or tape.
                        KeyCode::Up => {
                            let n = match view { Panel::Tape => tape.lock().unwrap().len(), Panel::Logs => bot.logs.len(),
                                // Orders may be filtered to the open token; scrolling
                                // past the end of what is drawn does nothing but feel broken.
                                _ => if !orders_all && !bot.pool.token.is_zero() {
                                    bot.orders.iter().filter(|o| o.token == bot.pool.token).count()
                                } else { bot.orders.len() } };
                            orders_scroll = (orders_scroll + 1).min(n.saturating_sub(1));
                        }
                        KeyCode::Down => { orders_scroll = orders_scroll.saturating_sub(1); }
                        // Refresh the live USD feed (ETH + stablecoin quotes) now.
                        KeyCode::Char('R') => {
                            // The recovery key: one press re-fetches everything
                            // that can go stale — a full market read on the next
                            // poll, tape cursors re-anchored so trades resume,
                            // fresh USD prices (in the background), and the
                            // pool's metadata. For when a hiccup leaves any
                            // panel behind and waiting feels wrong.
                            bot.status = "refreshing everything…".into();
                            force_full.store(true, Ordering::Relaxed);
                            tape_relive.store(true, Ordering::Relaxed);
                            let (ids, cell) = (feed_ids.clone(), feed_cell.clone());
                            tokio::spawn(async move {
                                let f = pricing::fetch_usd(&ids).await;
                                if !f.is_empty() {
                                    *cell.lock().unwrap() = Some(f);
                                }
                            });
                            refresh_token_decimals(provider, bot).await;
                            refresh_venue_meta(provider, bot).await;
                            bot.status = "refreshed — full read + tape re-anchor queued".into();
                        }
                        // Live knobs, shown as percentages:
                        //   [ ]  BUY size (% of ETH balance)    — see `buy_step`
                        //   ( )  SELL size (% of token balance) — 10% steps
                        //   { }  impact cap (% price move);  0 = off
                        KeyCode::Char(']') => {
                            let step = buy_step(bot);
                            // Snap to the step's grid, so a size set under a
                            // coarse step does not leave every later press
                            // landing on 1.35%, 1.45%, 1.55%.
                            bot.buy_frac = (((bot.buy_frac / step).round() + 1.0) * step).min(1.0);
                            let msg = buy_size_status(bot);
                            setting(bot, "Buy size", pct_compact(bot.buy_frac), msg);
                        }
                        // `;` finer, `'` coarser — adjacent keys for the two
                        // directions of one setting, the way [ ] and ( ) pair.
                        // The wallet-size default is a starting point, not a
                        // rule: past a few thousand dollars 0.5% is a big jump,
                        // but 0.1% is a lot of presses to cross a whole percent.
                        KeyCode::Char(';') => nudge_buy_step(bot, false),
                        KeyCode::Char('\'') => nudge_buy_step(bot, true),
                        KeyCode::Char('[') => {
                            let step = buy_step(bot);
                            bot.buy_frac = (((bot.buy_frac / step).round() - 1.0) * step).max(step);
                            let msg = buy_size_status(bot);
                            setting(bot, "Buy size", pct_compact(bot.buy_frac), msg);
                        }
                        // Bracket family, paired with the header labels:
                        // [] buy · () sell · {} slippage · <> impact cap.
                        // `()` is sell again — slippage had taken it.
                        KeyCode::Char(')') => { bot.sell_frac = (bot.sell_frac + 0.10).min(1.0); setting(bot, "Sell size", format!("{:.0}%", bot.sell_frac * 100.0), format!("Sell size is now {:.0} percent of your {} balance", bot.sell_frac * 100.0, bot.pool.sym)); }
                        KeyCode::Char('(') => { bot.sell_frac = (bot.sell_frac - 0.10).max(0.10); setting(bot, "Sell size", format!("{:.0}%", bot.sell_frac * 100.0), format!("Sell size is now {:.0} percent of your {} balance", bot.sell_frac * 100.0, bot.pool.sym)); }
                        KeyCode::Char('}') => { bot.slippage_pct = (bot.slippage_pct + 1.0).min(50.0); setting(bot, "Slippage tolerance", format!("{:.0}%", bot.slippage_pct), format!("Slippage tolerance is now {:.0} percent", bot.slippage_pct)); }
                        KeyCode::Char('{') => { bot.slippage_pct = (bot.slippage_pct - 1.0).max(1.0); setting(bot, "Slippage tolerance", format!("{:.0}%", bot.slippage_pct), format!("Slippage tolerance is now {:.0} percent", bot.slippage_pct)); }
                        KeyCode::Char('>') => { bot.max_price_move = (bot.max_price_move + 0.005).min(0.50); setting(bot, "Max price move", format!("{:.1}%", bot.max_price_move * 100.0), format!("A single swap may now move the price at most {:.1} percent", bot.max_price_move * 100.0)); }
                        KeyCode::Char('<') => { bot.max_price_move = (bot.max_price_move - 0.005).max(0.0); setting(bot, "Max price move", format!("{:.1}%", bot.max_price_move * 100.0), format!("A single swap may now move the price at most {:.1} percent", bot.max_price_move * 100.0)); }
                        KeyCode::Char('0') => { bot.max_price_move = 0.0; bot.status = "Price impact limit is off, swaps now go out at full size".into(); }
                        // Toggle the profitability filter — off lets you force genuine buys/sells.
                        KeyCode::Char('g') => { bot.profit_guard = !bot.profit_guard; bot.status = if bot.profit_guard {
                                "Profit filter is on, trades that would lose money are held back".into()
                            } else {
                                "Profit filter is off, be careful, losing trades are no longer blocked".to_string()
                            }; }
                        KeyCode::Char('n') => { bot.guard_dup = !bot.guard_dup; bot.status = if bot.guard_dup {
                                "Duplicate guard is on, the same buy will not repeat".into()
                            } else {
                                "Duplicate guard is off, be careful, the same buy can repeat".to_string()
                            }; }
                        // Clear the board. Everything on screen — pool, tape,
                        // position, routes, arb pair — belonged to a coin you are
                        // done with, and a half-cleared screen is worse than none:
                        // it still trades.
                        KeyCode::Delete => {
                            events::action("Cleared the selected pool", &[
                                ("was", bot.pool.sym.clone()),
                                ("token", format!("{:#x}", bot.pool.token)),
                            ]);
                            let blank = blank_pool(&bot.net.clone());
                            bot.pool = to_poolcfg(&blank);
                            bot.routes.clear();
                            bot.restore_basis();
                            bot.recover_basis(block.load(Ordering::Relaxed));
                            bot.lp_permit2_done = false;
                            bot.v3_covered = false;
                            bot.ur_permit2_done = false;
                            bot.socials = Default::default();
                            bot.pool_launch_block = None;
                            bot.arb_mode = false;
                            bot.pool_b = None;
                            *pool_b_cell.lock().unwrap() = None;
                            *pool_cell.lock().unwrap() = bot.pool.as_ref();
                            prices.clear();
                            bot.status = "pool deselected — press [f] to find pools".into();
                        }
                        KeyCode::Char('b') => { bot.status = "buying…".into(); let _ = tokio::time::timeout(Duration::from_secs(5), bot.place(provider, Side::Buy)).await; }
                        KeyCode::Char('s') => { bot.status = "selling…".into(); let _ = tokio::time::timeout(Duration::from_secs(5), bot.place(provider, Side::Sell)).await; }
                        KeyCode::Char('a') => { bot.status = "adding LP…".into(); let wei = (bot.eth * bot.lp_frac * 1e18).max(0.0) as u128; let _ = tokio::time::timeout(Duration::from_secs(8), bot.add_liquidity(provider, wei)).await; }
                        KeyCode::Char('r') => { bot.status = "removing one LP…".into(); let _ = tokio::time::timeout(Duration::from_secs(8), bot.remove_liquidity(provider)).await; }
                        KeyCode::Char('x') => { bot.status = "closing ALL LP…".into(); let _ = tokio::time::timeout(Duration::from_secs(12), bot.close_all(provider)).await; }
                        // The PnL calendar. Reads the fill ledger and nothing
                        // else — no RPC, no wallet — so opening it cannot cost
                        // a trade and it works with the network down.
                        KeyCode::Char('L') => {
                            pnl::screen(terminal)?;
                            bot.status = "back from PnL".into();
                        }
                        KeyCode::Char('S') => {
                            // Liquidate ALL token holdings: sweep every known v3 pool
                            // and sell any nonzero balance. Restores the active pool after.
                            bot.status = "liquidating ALL token holdings…".into();
                            let saved = bot.pool.clone();
                            let saved_routes = std::mem::take(&mut bot.routes);
                            let saved_covered = bot.v3_covered;
                            let saved_ur = bot.ur_permit2_done;
                            let mut swept = 0u32;
                            let mut seen: std::collections::HashSet<alloy::primitives::Address> = std::collections::HashSet::new();
                            for p in pools.clone() {
                                if !p.kind.is_v3() || !seen.insert(p.token) { continue; }
                                let bal = contracts::IERC20::new(p.token, provider)
                                    .balanceOf(bot.trader).call().await.map(|b| b._0).unwrap_or_default();
                                if bal.is_zero() { continue; }
                                bot.pool = to_poolcfg(&p);
                                trace_pool("switch", &bot.pool);
                                bot.routes = routes_for(&pools, p.token);
                                bot.v3_covered = false;
                                bot.ur_permit2_done = false;
                                let _ = tokio::time::timeout(Duration::from_secs(10), bot.sell_all(provider)).await;
                                swept += 1;
                            }
                            bot.pool = saved;
                            bot.routes = saved_routes;
                            bot.v3_covered = saved_covered;
                            bot.ur_permit2_done = saved_ur;
                            // Read the token real decimals BEFORE publishing to the market
                            // reader, so its first read is scaled correctly.
                            refresh_token_decimals(provider, bot).await;
                            *pool_cell.lock().unwrap() = bot.pool.as_ref();
                            bot.status = format!("liquidate: swept {swept} holding(s)");
                        }
                        KeyCode::Char('h') => {
                            // Wallet holdings: leftover tokens you still hold, with live
                            // ETH value — spot forgotten winners/dust. Enter to trade/sell.
                            bot.status = "reading wallet holdings…".into();
                            let trader = bot.trader;
                            let mut seen = std::collections::HashSet::new();
                            let cands: Vec<SelPool> = pools
                                .iter()
                                .filter(|p| p.kind.is_v3() && seen.insert(p.token))
                                .cloned()
                                .collect();
                            let rows: Vec<(SelPool, f64, f64)> = futures::stream::iter(cands)
                                .map(|p| async move {
                                    // Decimals must be read per token: dividing a
                                    // 6-dec balance (USDG) by 1e18 puts it below the
                                    // "not held" threshold, so real holdings vanish.
                                    let erc = contracts::IERC20::new(p.token, provider);
                                    let dec = erc
                                        .decimals()
                                        .call()
                                        .await
                                        .map(|d| d._0)
                                        .ok()
                                        .filter(|d| (1..=36).contains(d))
                                        .unwrap_or(18);
                                    let bal = erc
                                        .balanceOf(trader)
                                        .call()
                                        .await
                                        .map(|b| {
                                            b._0.to_string().parse::<f64>().unwrap_or(0.0)
                                                / 10f64.powi(dec as i32)
                                        })
                                        .unwrap_or(0.0);
                                    if bal <= 1e-9 {
                                        return (p, 0.0, 0.0); // not held — skip the price call
                                    }
                                    // Only held tokens pay for a price read (→ ETH value).
                                    let (sqrt, w0) = match p.kind {
                                        engine::PoolKind::V3 { pool_addr, weth_is_token0 } => {
                                            let s = contracts::IV3Pool::new(pool_addr, provider)
                                                .slot0()
                                                .call()
                                                .await
                                                .map(|r| r.sqrtPriceX96.to_string().parse::<f64>().unwrap_or(0.0) / 2f64.powi(96))
                                                .unwrap_or(0.0);
                                            (s, weth_is_token0)
                                        }
                                        _ => (0.0, false),
                                    };
                                    let p_raw = sqrt * sqrt;
                                    let tpe = if w0 { p_raw } else if p_raw > 0.0 { 1.0 / p_raw } else { 0.0 };
                                    // sqrtPriceX96 is a ratio of BASE units, so converting
                                    // it to ETH-per-whole-token needs the decimal gap
                                    // between the token (dec) and WETH (18).
                                    let ept_raw = if tpe > 0.0 { 1.0 / tpe } else { 0.0 };
                                    let ept = ept_raw * 10f64.powi(dec as i32 - 18);
                                    (p, bal, bal * ept)
                                })
                                .buffered(24)
                                .collect()
                                .await;
                            let mut held: Vec<(SelPool, f64, f64)> =
                                rows.into_iter().filter(|(_, bal, _)| *bal > 1e-9).collect();
                            held.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
                            if held.is_empty() {
                                bot.status = "no leftover tokens in wallet".into();
                            } else {
                                let labels: Vec<String> = held
                                    .iter()
                                    .map(|(p, bal, val)| format!("{:<12} {:>14.2}  ~{:.6} ETH", p.sym, bal, val))
                                    .collect();
                                if let Some(i) = ui::select(terminal, "Wallet holdings — Enter to trade/sell", &labels)? {
                                    let p = held[i].0.clone();
                                    bot.pool = to_poolcfg(&p);
                                    trace_pool("switch", &bot.pool);
                                    bot.restore_basis();
                                    bot.recover_basis(block.load(Ordering::Relaxed));
                            bot.recover_basis(block.load(Ordering::Relaxed));
                                    bot.lp_permit2_done = false;
                                    bot.v3_covered = false;
                                    bot.ur_permit2_done = false;
                                    apply_prices(bot, &feed);
                                    // Read the token's real decimals BEFORE publishing to the
                                    // market reader, so its first read is correctly scaled.
                                    // Read the token real decimals BEFORE publishing to the market
                                    // reader, so its first read is scaled correctly.
                                    refresh_token_decimals(provider, bot).await;
                                    *pool_cell.lock().unwrap() = bot.pool.as_ref();
                                    prices.clear();
                                    refresh_venue_meta(provider, bot).await;
                                    bot.status = format!("Now trading {}", pool_sentence(&p.label));
                                    bot.routes = routes_for(&pools, bot.pool.token);
                                }
                            }
                        }
                        // Arb mode: pick a 2nd pool (same token, other venue) to
                        // watch side-by-side with a live price gap. Press again to exit.
                        KeyCode::Char('d') => {
                            if bot.arb_mode {
                                bot.arb_mode = false;
                                bot.pool_b = None;
                                *pool_b_cell.lock().unwrap() = None;
                                bot.status = "Arb mode is off".into();
                            } else {
                                // Arb is ONE token across TWO venues — offer only
                                // already-added pools for the SAME token, minus the
                                // current pool A.
                                let cands: Vec<usize> = pools
                                    .iter()
                                    .enumerate()
                                    .filter(|(_, p)| p.token == bot.pool.token && p.kind != bot.pool.kind)
                                    .map(|(i, _)| i)
                                    .collect();
                                if cands.is_empty() {
                                    bot.status = format!("There is no second {} pool. Add one with p first", bot.pool.sym);
                                } else {
                                    let labels: Vec<String> = cands.iter().map(|&i| pools[i].label.clone()).collect();
                                    if let Some(sel) = ui::select(terminal, "Arb: 2nd pool (same token)", &labels)? {
                                        let i = cands[sel];
                                        let cfg = to_poolcfg(&pools[i]);
                                        *pool_b_cell.lock().unwrap() = Some(cfg.as_ref());
                                        bot.pool_b = Some(cfg);
                                        bot.arb_mode = true;
                                        apply_prices(bot, &feed); // value the new pool's quote
                                        bot.status = format!("arb: B = {}", pools[i].label);
                                    }
                                }
                            }
                        }
                        // Execute the two-leg arb: buy cheap pool, sell dear pool.
                        KeyCode::Char('e') => {
                            bot.status = "arb: executing…".into();
                            let _ = tokio::time::timeout(Duration::from_secs(14), bot.arb(provider)).await;
                        }
                        KeyCode::Char('f') | KeyCode::Char('F') | KeyCode::Char('k') => {
                            // 'f' = live Pons v3 trenches; Shift-'F' = static Verified pools;
                            // 'k' = top tokens (leaderboard + big-fish; parked here while
                            // 't' belongs to the Trades panel — pending a rethink).
                            // Pause the dashboard poll while a modal owns the
                            // screen — its numbers are invisible, and discovery
                            // needs the requests more.
                            poll_paused.store(true, Ordering::Relaxed);
                            let grad = if k.code == KeyCode::Char('F') {
                                bot.status = "Loading verified tokens".into();
                                discover::screen_verified(terminal, verified.clone()).await?
                            } else if k.code == KeyCode::Char('k') {
                                bot.status = "loading top tokens…".into();
                                discover::screen_top_tokens(terminal, provider, discovery_rpc.clone(), bot.eth_usd).await?
                            } else {
                                bot.status = "discovering token launches…".into();
                                discover::screen(terminal, provider, bot.trader, discovery_rpc.clone(), bot.eth_usd, verified.clone()).await?
                            };
                            poll_paused.store(false, Ordering::Relaxed);
                            match grad {
                                Some(g) => {
                                    let quote_sym = match &g.quote {
                                        engine::Quote::Eth => "ETH".to_string(),
                                        engine::Quote::Stable { token, .. } => stable_symbol(*token),
                                    };
                                    let p = SelPool {
                                        label: pool_label(false, g.kind.proto(), &quote_sym, &g.sym, g.fee, ""),
                                        kind: g.kind,
                                        token: g.token, sym: g.sym.clone(), fee: g.fee, owned: false,
                                        quote: g.quote, quote_sym,
                                    };
                                    bot.pool = to_poolcfg(&p);
                                    trace_pool("switch", &bot.pool);
                                    bot.socials = g.socials.clone(); // socials already read during discovery
                                    bot.pool_launch_block = Some(g.launch_block); // for the age display
                                    // A picked token stays on the discovery list even after it
                                    // stops being a fresh graduation — so it is still there
                                    // when you come back from trading it.
                                    if let engine::PoolKind::V3 { pool_addr, .. } = g.kind {
                                        discover::remember(g.token, pool_addr, g.launch_block);
                                    }
                                    bot.restore_basis(); // basis is per-token, and survives restarts
                                    bot.recover_basis(block.load(Ordering::Relaxed));
                                    bot.lp_permit2_done = false;
                                    bot.v3_covered = false;
                                    bot.ur_permit2_done = false;
                                    apply_prices(bot, &feed);
                                    // Read the token's real decimals BEFORE publishing to the
                                    // market reader, so its first read is correctly scaled.
                                    // Read the token real decimals BEFORE publishing to the market
                                    // reader, so its first read is scaled correctly.
                                    refresh_token_decimals(provider, bot).await;
                                    *pool_cell.lock().unwrap() = bot.pool.as_ref();
                                    prices.clear();
                                    bot.status = format!("Now trading {}", pool_sentence(&p.label));
                                    events::action("Loaded pool", &pool_facts(&bot.pool));
                                    match persist_pool(&network, &p) {
                                        Ok(()) => {
                                            bot.note(format!("Saved {} to your pool list", pool_sentence(&p.label)))
                                        }
                                        Err(e) => {
                                            events::error("Could not save pool; keeping it for this session", &[("reason", e.to_string())]);
                                            bot.note(format!("Kept {} for this session only because saving failed. {e}", pool_sentence(&p.label)))
                                        }
                                    }
                                    if !pools.iter().any(|q| q.label == p.label) {
                                        pools.push(p);
                                    }
                                    bot.routes = routes_for(&pools, bot.pool.token);
                                    // Land on the Tape — the new pool's live flow.
                                    view = Panel::Tape;
                                    orders_scroll = 0;
                                }
                                None => bot.status = "discovery cancelled".into(),
                            }
                        }
                        KeyCode::Char('p') => {
                            let mut labels = vec![
                                "＋ Add token by contract address".to_string(),
                                "＋ Add pool (select assets)".to_string(),
                                "＋ Create new v4 pool (select assets)".to_string(),
                            ];
                            // Prune sold-out tokens from the picker: keep pools we still
                            // hold a real balance of, plus any we own (LP). A SELL-ALL often
                            // leaves a few wei of dust (rounding / post-tx airdrops), so we
                            // treat anything below ~0.001 token (18-dec) as empty rather than
                            // exact-zero — otherwise sold pools linger. Concurrent reads,
                            // order preserved; on a read error we keep the pool.
                            let trader = bot.trader;
                            // 1e15 wei = 0.001 token at 18 decimals (these launch tokens are 18-dec).
                            let dust = alloy::primitives::U256::from(1_000_000_000_000_000u64);
                            // The registry accumulates every pool ever traded (hundreds), so this
                            // is a big burst of balance reads. Parallelize hard and bound each call
                            // so the menu opens fast; a slow/failed read keeps the pool (never hide
                            // a real holding behind a timeout).
                            let flags: Vec<bool> = futures::stream::iter(pools.iter().cloned())
                                .map(|p| async move {
                                    if p.owned { return true; }
                                    let erc = contracts::IERC20::new(p.token, provider);
                                    match tokio::time::timeout(
                                        std::time::Duration::from_millis(1500),
                                        erc.balanceOf(trader).call(),
                                    ).await {
                                        Ok(Ok(b)) => b._0 > dust,
                                        _ => true,
                                    }
                                })
                                .buffered(64)
                                .collect()
                                .await;
                            let visible: Vec<SelPool> = pools.iter().cloned()
                                .zip(flags).filter_map(|(p, keep)| keep.then_some(p)).collect();
                            labels.extend(visible.iter().map(|p| p.label.clone()));
                            if let Some(i) = ui::select(terminal, "Pools", &labels)? {
                                // Optionally produce a new SelPool to switch to + append.
                                let new_pool: Option<SelPool> = match i {
                                    0 => {
                                        // Paste a token address → auto-find its liquid WETH
                                        // v3 pool. Fast path for tokens found in the wild.
                                        match ui::input(terminal, "Add token by contract address", "paste the CA (0x…, 20 bytes), or a 32-byte Flaunch pool id")? {
                                            Some(s) => match s.trim().parse::<alloy::primitives::Address>() {
                                                Ok(token) => {
                                                    let sym = read_symbol(provider, token).await;
                                                    if let Some((addr, fee, w0)) = find_v3_pool(provider, token).await {
                                                        bot.status = format!("found v3 {} pool for {sym}", fee_label(fee));
                                                        Some(SelPool {
                                                            label: pool_label(false, "v3", "ETH", &sym, fee, ""),
                                                            kind: engine::PoolKind::V3 { pool_addr: addr, weth_is_token0: w0 },
                                                            token, sym, fee, owned: false,
                                                            quote: engine::Quote::Eth, quote_sym: "ETH".to_string(),
                                                        })
                                                    } else if let Some(fl) = discover::fetch_flaunch_pool(provider, token).await {
                                                        // No v3 pool, but Flaunch launched it — trade the
                                                        // flETH-paired hook pool instead.
                                                        bot.status = format!("found Flaunch pool for {sym}");
                                                        Some(SelPool {
                                                            label: pool_label(false, "flaunch", "ETH", &sym, contracts::FLAUNCH_FEE_EST, ""),
                                                            kind: engine::PoolKind::FlaunchV4 { pool_id: fl.pool_id, coin_is_0: fl.coin_is_0 },
                                                            token, sym, fee: contracts::FLAUNCH_FEE_EST, owned: false,
                                                            quote: engine::Quote::Eth, quote_sym: "ETH".to_string(),
                                                        })
                                                    } else {
                                                        bot.status = format!("No liquid Uniswap V3 pool for {sym}. Try selecting assets to use V4");
                                                        None
                                                    }
                                                }
                                                // Not a 20-byte address. Flaunch listings show the
                                                // 32-byte POOL ID as often as the coin's CA, and the
                                                // launch event is indexed by it — so a pool id
                                                // resolves too, back to the coin it belongs to.
                                                Err(_) => match s.trim().parse::<B256>() {
                                                    Ok(id) => {
                                                        if let Some((token, fl)) = discover::fetch_flaunch_by_id(provider, id).await {
                                                            let sym = read_symbol(provider, token).await;
                                                            bot.status = format!("found Flaunch pool for {sym}");
                                                            Some(SelPool {
                                                                label: pool_label(false, "flaunch", "ETH", &sym, contracts::FLAUNCH_FEE_EST, ""),
                                                                kind: engine::PoolKind::FlaunchV4 { pool_id: fl.pool_id, coin_is_0: fl.coin_is_0 },
                                                                token, sym, fee: contracts::FLAUNCH_FEE_EST, owned: false,
                                                                quote: engine::Quote::Eth, quote_sym: "ETH".to_string(),
                                                            })
                                                        } else {
                                                            // A plain v4 pool id has no launch log to
                                                            // find — the key can't be recovered from a
                                                            // hash, so those still need the pair picker.
                                                            bot.status = "No Flaunch launch has that pool id. For a plain v4 pool, add it by pair instead".into();
                                                            None
                                                        }
                                                    }
                                                    Err(_) => { bot.status = "invalid address".into(); None }
                                                },
                                            },
                                            None => None,
                                        }
                                    }
                                    1 => {
                                        // Add an existing pool for a wallet-selected pair.
                                        match pick_pair(terminal, provider, &assets, bot.trader).await? {
                                            Pick::Token(token, sym) => fee_tier_select(terminal)?.map(|(fee, spacing)| SelPool {
                                                    label: pool_label(false, "v4", "ETH", &sym, fee, ""),
                                                    kind: engine::PoolKind::V4 { pool_id: compute_pool_id(token, fee, spacing), tick_spacing: spacing },
                                                    token, sym, fee, owned: false,
                                                    quote: engine::Quote::Eth, quote_sym: "ETH".to_string(),
                                                }),
                                            Pick::NeedsEth => { bot.status = "select ETH + one token".into(); None }
                                            Pick::Cancelled => None,
                                        }
                                    }
                                    2 => {
                                        // Create (initialize) a new v4 pool for a selected pair.
                                        match pick_pair(terminal, provider, &assets, bot.trader).await? {
                                            Pick::Token(token, sym) => match fee_tier_select(terminal)? {
                                                Some((fee, spacing)) => {
                                                    let price = ui::input(terminal, "Initial price (token per ETH)", "e.g. 1.0")?
                                                        .and_then(|s| s.parse::<f64>().ok())
                                                        .unwrap_or(1.0);
                                                    let sp96 = price_to_sqrtx96(price);
                                                    let _ = tokio::time::timeout(Duration::from_secs(10), bot.initialize_pool(provider, token, fee, spacing, sp96)).await;
                                                    Some(SelPool {
                                                        label: pool_label(true, "v4", "ETH", &sym, fee, ""),
                                                        kind: engine::PoolKind::V4 { pool_id: compute_pool_id(token, fee, spacing), tick_spacing: spacing },
                                                        token, sym, fee, owned: true,
                                                        quote: engine::Quote::Eth, quote_sym: "ETH".to_string(),
                                                    })
                                                }
                                                None => None,
                                            },
                                            Pick::NeedsEth => { bot.status = "select ETH + one token".into(); None }
                                            Pick::Cancelled => None,
                                        }
                                    }
                                    _ => Some(visible[i - 3].clone()),
                                };
                                if let Some(p) = new_pool {
                                    bot.pool = to_poolcfg(&p);
                                    trace_pool("switch", &bot.pool);
                                    bot.restore_basis(); // basis is per-token, and survives restarts
                                    bot.recover_basis(block.load(Ordering::Relaxed));
                                    bot.lp_permit2_done = false;
                                    bot.v3_covered = false;
                                    bot.ur_permit2_done = false;
                                    apply_prices(bot, &feed); // value the new pool's quote
                                    // Read the token's real decimals BEFORE publishing to the
                                    // market reader, so its first read is correctly scaled.
                                    // Read the token real decimals BEFORE publishing to the market
                                    // reader, so its first read is scaled correctly.
                                    refresh_token_decimals(provider, bot).await;
                                    *pool_cell.lock().unwrap() = bot.pool.as_ref();
                                    prices.clear();
                                    bot.status = format!("Now trading {}", pool_sentence(&p.label));
                                    events::action("Loaded pool", &pool_facts(&bot.pool));
                                    if i < 3 {
                                        // Persist created/added pools to the registry so
                                        // they survive restarts (not just this session).
                                        match persist_pool(&network, &p) {
                                            Ok(()) => {
                                                bot.note(format!("Saved {} to your pool list", pool_sentence(&p.label)))
                                            }
                                            Err(e) => {
                                                events::error("Could not save pool; keeping it for this session", &[("reason", e.to_string())]);
                                                bot.note(format!("Kept {} for this session only because saving failed. {e}", pool_sentence(&p.label)))
                                            }
                                        }
                                        pools.push(p);
                                    }
                                    // Refresh venues AFTER the new pool is in `pools`, so a
                                    // freshly-added pool is itself a routing candidate.
                                    bot.routes = routes_for(&pools, bot.pool.token);
                                    refresh_venue_meta(provider, bot).await;
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

/// Which panel fills the middle of the dashboard (cycled with 'l').
#[derive(Clone, Copy, PartialEq)]
enum Panel {
    Orders, // our own actions
    Tape,   // all traders' swaps on the pool
    Logs,   // raw session log
    Chart,  // the tape re-read as candles — same data, TradingView grammar
}


/// Draws the dashboard and returns where the header logo goes, so the caller
/// can place a real terminal image there after the frame.
#[allow(clippy::too_many_arguments)] // a swap needs every one of these; bundling them into a struct would only move the list
fn draw(f: &mut Frame, bot: &Bot, block: u64, round_ms: f64, view: Panel, orders_scroll: usize, orders_all: bool, tape: &[engine::Swap], show_help: bool) -> Option<Rect> {
    // Paint the theme background FIRST. Without this a light theme renders dark
    // text on the terminal's own dark background — unreadable.
    ui::widgets::paint_bg(f);
    // One info row: [wallet | market] normally, [wallet | market A | market B]
    // in arb mode. Wallet keeps its familiar vertical format and stays on the left.
    let arb = bot.arb_mode;
    let c = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),  // header (logo + large venue type)
            Constraint::Length(14), // info row (fits wallet's 12 lines incl. status + last fill)
            Constraint::Length(7),  // settings — every adjustable knob on its
                                    // own row, with the latest message beneath
            Constraint::Min(4),     // orders/tape/logs
            Constraint::Length(3),  // footer
        ])
        .split(f.area());
    let info_area = c[1];
    let status_area = c[2];
    let mid_area = c[3];
    let foot_area = c[4];

    // Under 200ms is a healthy round trip to a remote node — the old 10ms floor
    // meant a perfectly good connection always showed amber.
    let lat_color = if round_ms < 200.0 {
        ui::widgets::tone_color(view::Tone::Good)
    } else if round_ms < 800.0 {
        ui::widgets::tone_color(view::Tone::Warn)
    } else {
        ui::widgets::tone_color(view::Tone::Bad)
    };
    // Left: the venue in large type beside its mark. Right: live chain state.
    // The knobs moved to their own Settings box, which freed these rows.
    let (logo_box, indent_cols) = ui::image::header_box(ui::widgets::themed_block("").inner(c[0]));
    let venue = header_venue(bot);
    let avail = c[0].width.saturating_sub(indent_cols + 32);
    // Trading says TRENCHES.SH; an empty screen says the chain.
    //
    // The venue's mark stays beside it — the logo is how you tell a Pons launch
    // from a Uniswap pool at a glance, and that is worth keeping. Its name in
    // large type is not: this is our screen, it ends up in screenshots, and
    // setting someone else's brand across it in the biggest type on the page
    // advertises them rather than us.
    //
    // The exception is the empty state, which names the chain. There is no
    // trade to label there, and the chain is the one thing still true.
    let name = match venue {
        ui::image::Venue::Chain => venue.display_name(&bot.net),
        // Only when it fits: on a narrow terminal the full name drops out of
        // large type and renders as small text, which is a worse trade than a
        // short form set properly.
        _ if ui::bigtext::width("TRENCHES.SH") <= avail => "TRENCHES.SH".to_string(),
        _ => "TRENCHES".to_string(),
    };

    // Large type only if it fits; on a narrow terminal the plain name is
    // better than three rows of clipped blocks.
    let name_style = Style::default()
        .fg(ui::widgets::tone_color(view::Tone::Accent))
        .add_modifier(Modifier::BOLD);
    let head_left = if ui::bigtext::width(&name) <= avail {
        Paragraph::new(ui::bigtext::render(&name, name_style))
    } else {
        Paragraph::new(Line::from(Span::styled(name.clone(), name_style)))
    };

    let head_right = Paragraph::new(vec![
        // The chain leads: it is the thing that never changes while the two
        // below it change constantly, so it anchors the column.
        Line::from(vec![Span::styled(
            format!("{} ", bot.net),
            Style::default().fg(ui::widgets::border_color()).add_modifier(Modifier::BOLD),
        )]),
        // Value first, label last: right-aligned, that puts the labels flush
        // against the edge as a column you read down, with the numbers beside
        // them. Labels take the primary colour, like every other label.
        Line::from(vec![
            Span::styled(format!("{round_ms:.0}ms "), Style::default().fg(lat_color)),
            Span::styled(
                "latency ",
                Style::default().fg(ui::widgets::border_color()).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::raw(format!("{block} ")),
            Span::styled(
                "block ",
                Style::default().fg(ui::widgets::border_color()).add_modifier(Modifier::BOLD),
            ),
        ]),
    ])
    .alignment(Alignment::Right);

    let head_block = ui::widgets::themed_block(format!(" Trenches Bot v{} ", update::current()));
    let head_inner = head_block.inner(c[0]);
    f.render_widget(head_block, c[0]);
    let head_cols = Layout::horizontal([
        Constraint::Length(indent_cols),
        Constraint::Min(20),
        Constraint::Length(30),
    ])
    .split(head_inner);
    f.render_widget(head_left, head_cols[1]);
    f.render_widget(head_right, head_cols[2]);

    // Real image where the terminal supports one; block art is the fallback.
    if !ui::image::supported() {
        if let Some(l) = ui::logo::for_venue(venue, &bot.net) {
            f.render_widget(
                Paragraph::new(l.render_fit(logo_box.width, logo_box.height)),
                logo_box,
            );
        }
    }

    // Everything adjustable in one box, grouped by what it does, with the most
    // recent message underneath — so "what can I change" and "what just
    // happened" are one place instead of scattered across the header.
    // Key hints share the border colour: they are chrome that tells you what to
    // press, distinct from the values they act on.
    let hint = |k: &'static str| Span::styled(
        k,
        Style::default().fg(ui::widgets::border_color()).add_modifier(Modifier::BOLD),
    );
    // Labels take the primary colour with the keys; the VALUES stay normal text
    // so the number you are reading is the thing that stands out against them.
    let sbold = |t: String| Span::styled(
        t,
        Style::default().fg(ui::widgets::border_color()).add_modifier(Modifier::BOLD),
    );
    let val = |t: String| Span::styled(t, Style::default().fg(ui::widgets::tone_color(view::Tone::Normal)));
    f.render_widget(
        Paragraph::new(vec![
            // Left column is what you tune while trading; right column is the
            // two modes that change how it behaves. Grouping them that way
            // beats one long ragged list.
            Line::from(vec![
                hint("[ ] "),
                sbold(format!("{:<10}", "buy")),
                val(format!("{:<10}", pct_compact(bot.buy_frac))),
                hint("    "),
                sbold(format!("{:<10}", "mode")),
                val("manual".to_string()),
            ]),
            Line::from(vec![
                hint("( ) "),
                sbold(format!("{:<10}", "sell")),
                val(format!("{:<10}", format!("{:.0}%", bot.sell_frac * 100.0))),
                hint("[g] "),
                sbold(format!("{:<10}", "guard")),
                Span::styled(
                    if bot.profit_guard { "ON" } else { "OFF" },
                    Style::default()
                        .fg(if bot.profit_guard {
                            ui::widgets::tone_color(view::Tone::Good)
                        } else {
                            ui::widgets::tone_color(view::Tone::Bad)
                        })
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                hint("{ } "),
                sbold(format!("{:<10}", "slippage")),
                val(format!("{:<10}", format!("{:.0}%", bot.slippage_pct))),
                // Under the guard it sits beside, because they are the same kind
                // of switch: both refuse a trade rather than shaping one.
                hint("[n] "),
                sbold(format!("{:<10}", "dedup")),
                Span::styled(
                    if bot.guard_dup { "ON" } else { "OFF" },
                    Style::default()
                        .fg(if bot.guard_dup {
                            ui::widgets::tone_color(view::Tone::Good)
                        } else {
                            ui::widgets::tone_color(view::Tone::Bad)
                        })
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                hint("< > "),
                sbold(format!("{:<10}", "Impact")),
                val(if bot.max_price_move > 0.0 {
                    format!("{:.1}%", bot.max_price_move * 100.0)
                } else {
                    "off".into()
                }),
            ]),
            Line::from(vec![
                // Not padded to the settings column: it is a message, not a
                // value in that grid, so aligning it just opens a gap.
                Span::styled(
                    "Status  ",
                    Style::default().fg(ui::widgets::border_color()).add_modifier(Modifier::BOLD),
                ),
                // The highlight blue: the status is the one sentence the
                // dashboard is currently saying to you.
                Span::styled(bot.status.clone(), Style::default().fg(ui::widgets::tone_color(view::Tone::Info))),
            ]),
        ])
        .block(ui::widgets::themed_block(" Settings ")),
        status_area,
    );

    let pnl = bot.pnl();
    let pnl_color = if pnl >= 0.0 { ui::widgets::tone_color(view::Tone::Good) } else { ui::widgets::tone_color(view::Tone::Bad) };

    // Columns: [wallet | market] normally, [wallet | market A | market B] in arb.
    // Wallet is always column 0 (left).
    let cols = if arb {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(34), Constraint::Percentage(33), Constraint::Percentage(33)])
            .split(info_area)
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(info_area)
    };
    let (wallet_col, mkt_a_col, mkt_b_col) = (0usize, 1usize, 2usize);

    // Bold, fixed-width label + plain value, matching the Wallet column.
    let mlbl = |t: &str| {
        Span::styled(
            format!("{t:<9}"),
            Style::default().fg(ui::widgets::border_color()).add_modifier(Modifier::BOLD),
        )
    };
    let mut mkt: Vec<Line> = Vec::new();
    // Addresses first — always FULL so they can be copy-pasted into an explorer.
    mkt.push(Line::from(vec![mlbl("Token"), Span::raw(bot.pool.token.to_string())]));
    match bot.pool.kind {
        engine::PoolKind::V3 { pool_addr, .. } =>
            mkt.push(Line::from(vec![mlbl("Pool"), Span::raw(format!("{pool_addr}"))])),
        engine::PoolKind::PonsCurve { curve, .. } =>
            mkt.push(Line::from(vec![mlbl("Curve"), Span::raw(format!("{curve}"))])),
        engine::PoolKind::V4 { pool_id, .. }
        | engine::PoolKind::FlaunchV4 { pool_id, .. }
        | engine::PoolKind::PonsV2Pool { pool_id, .. } =>
            mkt.push(Line::from(vec![mlbl("Pool"), Span::raw(format!("{pool_id}"))])),
    }
    // Which venue and pair, first — it moved off the header to make room for
    // the large type, and it belongs with the rest of the pool's identity.
    mkt.insert(
        0,
        Line::from(vec![
            mlbl("Protocol"),
            Span::styled(
                format!("{} ETH/{} {}", bot.pool.kind.venue_label(), bot.pool.sym, fee_label(bot.pool.fee)),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]),
    );
    // One piece of information per row — priced in the pool's quote currency.
    mkt.push(Line::from(vec![mlbl("Price"), Span::raw(format!("{:.4} {}/{}", bot.price(), bot.pool.sym, bot.pool.quote_sym))]));
    mkt.push(Line::from(vec![mlbl("Tick"), Span::raw(format!("{}", bot.tick))]));
    mkt.push(Line::from(vec![mlbl("Mkt Cap"), Span::raw(format!("${:.2}M", bot.market_cap_usd() / 1e6))]));
    if let Some(lb) = bot.pons_launch() {
        let s = block.saturating_sub(lb) / 10; // ~10 blocks/sec since graduation
        let a = view::age_compact(s as f64);
        let since = if matches!(bot.pool.kind, engine::PoolKind::FlaunchV4 { .. }) {
            "since flaunch"
        } else {
            "since graduation"
        };
        mkt.push(Line::from(vec![mlbl("Age"), Span::raw(format!("{a} ({since})"))]));
    }
    // Pooled reserves (like dexscreener/gmgn) — each side its own row.
    // These come from L and the current price (`L/√P`, `L·√P`), which is what
    // the pool WOULD hold if its liquidity spanned the whole curve. For a
    // concentrated position that overstates real depth — sometimes past the
    // token's entire supply, which is the giveaway. Flag it when that happens
    // rather than presenting a number that cannot be true as exit liquidity.
    let notional = bot.token_supply > 0.0 && bot.r1 > bot.token_supply;
    mkt.push(Line::from(vec![mlbl("Pooled"), Span::raw(format!("{} {}", view::eth(bot.r0), bot.pool.quote_sym))]));
    mkt.push(Line::from(vec![
        mlbl("Pooled"),
        Span::styled(
            if notional {
                format!("{:.0} {} (estimate, above supply)", bot.r1, bot.pool.sym)
            } else {
                format!("{:.0} {}", bot.r1, bot.pool.sym)
            },
            Style::default().fg(if notional {
                ui::widgets::tone_color(view::Tone::Warn)
            } else {
                ui::widgets::tone_color(view::Tone::Normal)
            }),
        ),
    ]));
    // Token metadata (Pons socials) — confirm the details of what you're trading.
    if !bot.socials.is_empty() {
        mkt.push(Line::from(vec![mlbl("Socials"), Span::raw(format!("{}/7 filled", bot.socials.score()))])
            .style(Style::default().fg(ui::widgets::tone_color(view::Tone::Normal))));
        let m = &bot.socials;
        if !m.website.trim().is_empty() { mkt.push(Line::from(vec![mlbl("Web"), Span::raw(m.website.clone())])); }
        if !m.twitter.trim().is_empty() { mkt.push(Line::from(vec![mlbl("X"), Span::raw(m.twitter.clone())])); }
        if !m.telegram.trim().is_empty() { mkt.push(Line::from(vec![mlbl("Telegram"), Span::raw(m.telegram.clone())])); }
        if !m.discord.trim().is_empty() { mkt.push(Line::from(vec![mlbl("Discord"), Span::raw(m.discord.clone())])); }
    }
    // Nothing selected: none of the above is a fact. A zero address, a 0% fee
    // and a venue we are not on read as data, and the whole point of the empty
    // state is that there is none — so throw it away and say what to press.
    if bot.pool.token.is_zero() {
        mkt = vec![
            Line::from(""),
            Line::from(Span::styled(
                "  No pool selected",
                Style::default().fg(ui::widgets::tone_color(view::Tone::Normal)).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  [f] find pools in the trenches",
                Style::default().fg(ui::widgets::tone_color(view::Tone::Info)),
            )),
            Line::from(Span::styled(
                "  [k] top tokens",
                Style::default().fg(ui::widgets::tone_color(view::Tone::Info)),
            )),
            Line::from(Span::styled(
                "  [p] add a token by contract address",
                Style::default().fg(ui::widgets::tone_color(view::Tone::Info)),
            )),
        ];
    }
    // (profit / edge / status moved to the wallet panel — they're bot-wide.)
    // "Pool" — it is the pool you are trading, and `p` is what changes it.
    let mkt_title = if bot.arb_mode {
        format!(" Pool A [{}] ", bot.pool.kind.proto())
    } else {
        " Pool [p] ".to_string()
    };
    let market = Paragraph::new(mkt).block(ui::widgets::themed_block(mkt_title));
    f.render_widget(market, cols[mkt_a_col]);

    // Arb mode: second pool's own panel in the middle column.
    if bot.arb_mode {
        if let Some(pb) = bot.pool_b.as_ref() {
            let mut mb: Vec<Line> = Vec::new();
            mb.push(Line::from(Span::styled(format!("{:<9}{}", "Token", pb.token), Style::default().fg(ui::widgets::tone_color(view::Tone::Normal)))));
            match pb.kind {
                engine::PoolKind::V3 { pool_addr, .. } =>
                    mb.push(Line::from(vec![mlbl("Pool"), Span::raw(format!("{pool_addr}"))])),
                engine::PoolKind::PonsCurve { curve, .. } =>
                    mb.push(Line::from(vec![mlbl("Curve"), Span::raw(format!("{curve}"))])),
                engine::PoolKind::V4 { pool_id, .. }
                | engine::PoolKind::FlaunchV4 { pool_id, .. }
                | engine::PoolKind::PonsV2Pool { pool_id, .. } =>
                    mb.push(Line::from(vec![mlbl("Pool"), Span::raw(format!("{pool_id}"))])),
            }
            let pbp = bot.price_b();
            mb.push(Line::from(format!("{:<9}{:.4} {}/{}", "Price", pbp, pb.sym, pb.quote_sym)));
            mb.push(Line::from(format!("{:<9}{}", "Tick", bot.mkt_b.tick)));
            let mcap_b = if pbp > 0.0 { bot.mkt_b.supply / pbp * pb.quote_usd } else { 0.0 };
            mb.push(Line::from(format!("{:<9}${:.2}M", "Mkt Cap", mcap_b / 1e6)));
            mb.push(Line::from(format!("{:<9}{} {}", "Pooled", view::eth(bot.mkt_b.r0), pb.quote_sym)));
            mb.push(Line::from(format!("{:<9}{:.0} {}", "Pooled", bot.mkt_b.r1, pb.sym)));
            let gap = bot.arb_gap_pct();
            mb.push(Line::from(vec![
                Span::styled("gap A/B  ", Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(
                    format!("{gap:+.3}%"),
                    Style::default().fg(if gap.abs() > 0.5 { ui::widgets::tone_color(view::Tone::Good) } else { ui::widgets::tone_color(view::Tone::Warn) }).add_modifier(Modifier::BOLD),
                ),
            ]));
            // Precalculated best arb: optimal size + net profit (0 = not worth it).
            let (opt_in, opt_net) = bot.arb_optimal();
            if opt_net > 0.0 {
                mb.push(Line::from(vec![
                    Span::styled(format!("arb +{opt_net:.6} ETH", ), Style::default().fg(ui::widgets::tone_color(view::Tone::Good)).add_modifier(Modifier::BOLD)),
                    Span::styled(format!("  @ {opt_in:.4} in  (e=exec)", ), Style::default().fg(ui::widgets::tone_color(view::Tone::Dim))),
                ]));
            } else {
                mb.push(Line::from(Span::styled("arb  none — spread < fees", Style::default().fg(ui::widgets::tone_color(view::Tone::Dim)))));
            }
            let panel_b = Paragraph::new(mb)
                .block(ui::widgets::themed_block(format!("Market B [{}] {}", pb.kind.proto(), fee_label(pb.fee))));
            f.render_widget(panel_b, cols[mkt_b_col]);
        }
    }

    let day_color = if bot.daily_pnl() >= 0.0 { ui::widgets::tone_color(view::Tone::Good) } else { ui::widgets::tone_color(view::Tone::Bad) };
    let real_color = if bot.realized_pnl >= 0.0 { ui::widgets::tone_color(view::Tone::Good) } else { ui::widgets::tone_color(view::Tone::Bad) };
    // Live, slippage-aware unrealized P&L of selling the whole holding NOW —
    // updates every refresh, independent of the profit filter being on/off.
    let live = bot.live_edge();
    let edge_span = Span::styled(
        format!("{:+.7}", live),
        Style::default()
            .fg(if live > 0.0 { ui::widgets::tone_color(view::Tone::Good) } else if live < 0.0 { ui::widgets::tone_color(view::Tone::Bad) } else { ui::widgets::tone_color(view::Tone::Dim) })
            .add_modifier(Modifier::BOLD),
    );
    // Wallet — same vertical format in both modes, always on the left (col 0).
    // Row labels are bold so the eye lands on them first; values carry the
    // colour. `lbl` keeps the column alignment in one place.
    let lbl = |t: &str| {
        Span::styled(
            format!("{t:<11}"),
            Style::default().fg(ui::widgets::border_color()).add_modifier(Modifier::BOLD),
        )
    };
    // Nothing to report without an account.
    //
    // A row of zeroes is not "empty", it is a claim: zero balance, zero
    // realized, zero trades. None of that is known until a key is unlocked, and
    // showing it invites someone to read a wallet they have not opened. Same
    // shape as the Pool panel's empty state — say what is missing, then the key
    // that fixes it.
    if bot.trader.is_zero() {
        let empty = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                "  No account",
                Style::default()
                    .fg(ui::widgets::tone_color(view::Tone::Normal))
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  [W] unlock or create an account",
                Style::default().fg(ui::widgets::tone_color(view::Tone::Info)),
            )),
        ])
        .block(ui::widgets::themed_block(" Wallet [W] "));
        f.render_widget(empty, cols[wallet_col]);
    } else {
    // The PnL rows are denominated in the pool's quote currency, so they price
    // off the quote's own rate — for a stock pool that is USDG, not ETH. Falls
    // back to the ETH rate, which is what the quote is on every ETH pool.
    let quote_rate = if bot.pool.quote_usd > 0.0 { bot.pool.quote_usd } else { bot.eth_usd };
    // Label, value, colour — plus the dollar figure beside it, dimmed, so the
    // native number stays the one being read and the dollars are the aside.
    let pnl_row = |label: &str, v: f64, color| {
        let mut spans = vec![
            lbl(label),
            Span::styled(
                format!("{v:+.6} {}", bot.pool.quote_sym),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
        ];
        if let Some(tag) = view::usd_tag(v, quote_rate) {
            spans.push(Span::styled(
                format!("  {tag}"),
                Style::default().fg(ui::widgets::tone_color(view::Tone::Dim)),
            ));
        }
        Line::from(spans)
    };
    let wallet = Paragraph::new(vec![
        // "Which account am I?" belongs with the balances, not in the header.
        Line::from(vec![
            lbl("Account"),
            // A zero address is not an account, and printing forty characters of
            // zeroes says "something is wrong" rather than "you have not
            // unlocked one". Name the key that fixes it instead.
            if bot.trader.is_zero() {
                Span::styled(
                    "no account — press [W] to unlock one",
                    Style::default()
                        .fg(ui::widgets::tone_color(view::Tone::Warn))
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(
                    format!("{}", bot.trader),
                    Style::default().add_modifier(Modifier::BOLD),
                )
            },
        ]),
        Line::from({
            // The balance in the unit you spend, and beside it the unit you
            // think in — dimmed, approximate, absent when the price is.
            let mut spans = vec![lbl("ETH"), Span::raw(view::eth(bot.eth))];
            if bot.eth_usd > 0.0 && bot.eth > 0.0 {
                spans.push(Span::styled(
                    format!("  ({})", view::usd_compact(bot.eth * bot.eth_usd)),
                    Style::default().fg(ui::widgets::tone_color(view::Tone::Dim)),
                ));
            }
            spans
        }),
        Line::from(vec![lbl(&bot.pool.sym), Span::raw(format!("{:.4}", bot.token_bal))]),
        Line::from(vec![
            lbl("Our Liq"),
            Span::raw(format!("{} {} ({} pos)", view::eth(bot.our_liq_eth()), bot.pool.quote_sym, bot.positions.len())),
        ]),
        Line::from(vec![
            lbl("Basis"),
            Span::raw(format!("{:.6} {}/{}", bot.avg_basis(), bot.pool.quote_sym, bot.pool.sym)),
        ]),
        Line::from(vec![
            lbl("Inventory"),
            Span::raw(format!("{:.2} {} (bought)", bot.bought_qty, bot.pool.sym)),
        ]),
        pnl_row("Realized", bot.realized_pnl, real_color),
        match bot.last_fill_pnl {
            Some(v) => pnl_row(
                "Last Fill",
                v,
                if v >= 0.0 {
                    ui::widgets::tone_color(view::Tone::Good)
                } else {
                    ui::widgets::tone_color(view::Tone::Bad)
                },
            ),
            None => Line::from(vec![
                lbl("Last Fill"),
                Span::styled(
                    "—".to_string(),
                    Style::default()
                        .fg(ui::widgets::tone_color(view::Tone::Dim))
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
        },
        pnl_row("PnL/Day", bot.daily_pnl(), day_color),
        pnl_row("PnL/Sesh", pnl, pnl_color),
        // Profit above Activity: the guard and edge belong with the PnL lines
        // above them, and Activity reads as the running tally at the bottom.
        Line::from(vec![lbl("Edge"), edge_span]),
        Line::from(vec![
            lbl("Activity"),
            Span::raw(format!(
                "{} trades  {} fails  {} skips  {} pending",
                bot.trades, bot.fails, bot.skips, bot.pending.len()
            )),
        ]),
    ])
    .block(ui::widgets::themed_block(" Wallet [W] "));
    f.render_widget(wallet, cols[wallet_col]);
    }

    match view {
        Panel::Chart => {
            // The tape re-read as candles. EVM swaps carry a BLOCK, not a
            // wall-clock stamp — at ~10 blocks/sec, block/10 is a perfectly
            // good second for bucketing (only relative spacing matters).
            // Price per token in ETH is the INVERSE of the tape's
            // tokens-per-ETH, so up on the chart means up in price.
            let pool_is_v4 = matches!(bot.pool.kind, engine::PoolKind::V4 { .. } | engine::PoolKind::FlaunchV4 { .. });
            let points: Vec<(i64, f64, f64)> = tape
                .iter()
                .filter(|s| (bot.arb_mode || s.is_v4 == pool_is_v4) && s.price > 0.0)
                .map(|s| ((s.block / 10) as i64, 1.0 / s.price, s.eth))
                .collect();
            // "Now" on the same block/10 clock as the points, so five quiet
            // minutes read as a flat line up to the live head, not a freeze.
            let candles = view::candles_of(&points, bot.chart_iv, 240, Some((block / 10) as i64));
            // Our own fills: any tape row whose tx is in the persisted
            // own-transaction set, on the same block/10 clock as the points.
            // One entry line per FILL, not per tape row. A duplicated row drew a
            // second marker for the same trade at a different price, which reads
            // as an entry you never took.
            let mut seen_fill = std::collections::HashSet::new();
            let trades: Vec<(i64, f64, bool)> = tape
                .iter()
                .filter(|s| bot.own_txs.contains(&s.tx) && s.price > 0.0)
                .filter(|s| matches!(s.action, engine::TapeAction::Buy | engine::TapeAction::Sell))
                .filter(|s| seen_fill.insert(s.tx))
                .map(|s| ((s.block / 10) as i64, 1.0 / s.price, matches!(s.action, engine::TapeAction::Buy)))
                .collect();
            let cv = view::CandleView {
                title: format!(
                    " {}/ETH {} candle [,] [.] ",
                    bot.pool.sym,
                    view::iv_label(bot.chart_iv)
                ),
                candles,
                interval_secs: bot.chart_iv,
                unit: "ETH",
                active_key: Some('c'),
                trades,
            };
            ui::widgets::candles(f, mid_area, &cv);
        }
        Panel::Logs => {
        // Logs view: last N lines (fit to panel), errors/skips highlighted.
        let h = mid_area.height.saturating_sub(2) as usize;
        // Two streams, one screen: what the bot did (trades, state changes) and
        // what the machinery reported (scans, RPC failures). Both carry an
        // [HH:MM:SS] stamp, so a stable sort on the first ten characters puts
        // them back in the order they actually happened.
        // Events only. Every line here is one thing that happened — an action,
        // an order, a fill, a failure — with the details that make it
        // actionable. The poll stream (`market:` ten times a second, TELEMETRY
        // every two) still goes to the trace and session files in full, but it
        // has no business on the screen someone opens to find out what went
        // wrong: it buried every informative line under thousands that were not.
        let is_noise = |l: &str| {
            let body = l.get(11..).unwrap_or(l); // past the [HH:MM:SS] stamp
            body.starts_with("TELEMETRY") || body.starts_with("market:") || body.starts_with("pool ")
        };
        let mut all: Vec<String> = bot.logs.iter().filter(|l| !is_noise(l)).cloned().collect();
        all.extend(events::recent());
        all.sort_by(|a, b| a.chars().take(10).cmp(b.chars().take(10)));
        let scroll = orders_scroll.min(all.len().saturating_sub(1));
        let mut lines: Vec<Line> = Vec::new();
        for l in all.iter().rev().skip(scroll).take(h.max(1)).rev() {
            let color = if let Some(lvl) = events::Level::of(l) {
                ui::widgets::tone_color(match lvl {
                    events::Level::Error => view::Tone::Bad,
                    events::Level::Warn => view::Tone::Warn,
                    events::Level::Trade => view::Tone::Good,
                    events::Level::Action => view::Tone::Info,
                    events::Level::Info => view::Tone::Normal,
                })
            } else if l.contains("REVERT") || l.contains("FAIL") || l.contains("failed") || l.contains("error") {
                ui::widgets::tone_color(view::Tone::Bad)
            } else if l.contains("SKIP") || l.contains("skipped") || l.contains("WARN") || l.contains("would revert") {
                ui::widgets::tone_color(view::Tone::Warn)
            } else if l.contains("CONFIRMED") || l.contains("LIVE") {
                ui::widgets::tone_color(view::Tone::Good)
            } else {
                ui::widgets::tone_color(view::Tone::Normal)
            };
            lines.push(Line::from(Span::styled(l.clone(), Style::default().fg(color))));
        }
        if lines.is_empty() {
            lines.push(Line::from("  (no log lines yet)"));
        }
        let log_title = match session_log_name() {
            n if n.is_empty() => " Logs ".to_string(),
            n => format!(" Logs — ~/.trenches/{n} "),
        };
        let logs = Paragraph::new(lines)
            .block(ui::widgets::with_panel_menu(ui::widgets::themed_block(&log_title)));
        f.render_widget(logs, mid_area);
        }
        Panel::Tape => {
            // Live tape: every trader's swaps on the current pool, newest first.
            // Our own trades (tx hash matches an order) get a ★ marker.
            // The persisted own-transaction set: a buy made LAST session keeps
            // its mark next to this session's sell.
            let ours = &bot.own_txs;
            let h = mid_area.height.saturating_sub(3).max(1) as usize;
            // ~10 blocks/sec on Robinhood Chain — estimate age from block delta.
            let age = |blk: u64| -> String {
                // No block, or a block ahead of the head we have read: say so.
                // Subtracting from zero produces a confident, enormous, wrong
                // number, and "—" is the honest shape of "not known yet".
                if blk == 0 || blk > block {
                    return "—".to_string();
                }
                let secs = block.saturating_sub(blk) / 10;
                view::age_compact(secs as f64)
            };
            // Single-market mode shows only the active venue's swaps; arb mode
            // keeps the merged v3+v4 tape.
            // FlaunchV4 IS v4 — its swaps decode with is_v4 = true, from the same
            // PoolManager. Leaving it out of this match meant every Flaunch row
            // was fetched, decoded, stored, and then dropped one line before
            // rendering: the panel said "no trades on this pool yet" while the
            // trace said "1 swap(s)" for the very trade you had just made.
            let pool_is_v4 = matches!(
                bot.pool.kind,
                engine::PoolKind::V4 { .. } | engine::PoolKind::FlaunchV4 { .. }
            );
            // One row per TRANSACTION, whatever the log stream did.
            //
            // Belt and braces over the placeholder fix: a confirmed order is
            // injected before its log arrives, and a swap can also be seen
            // twice across an overlapping scan window. Either way the same
            // trade rendering twice reads as two trades — which is exactly the
            // shape that makes someone check whether their key has leaked.
            // The tape is a record you make decisions from; it must not
            // invent activity.
            let mut seen_tx = std::collections::HashSet::new();
            let shown: Vec<&engine::Swap> = tape
                .iter()
                .filter(|s| bot.arb_mode || s.is_v4 == pool_is_v4)
                .filter(|s| seen_tx.insert(s.tx))
                .collect();
            let scroll = orders_scroll.min(shown.len().saturating_sub(1));
            let mut t = view::TableView::new(
                format!(" Trades ({}) ⭐ = you ", shown.len()),
                vec![
                    view::Col::fixed("", 2),
                    // Age leads, as on the Solana tape: a tape is read
                    // newest-first, so "how long ago" is the first thing wanted.
                    view::Col::fixed("age", 6),
                    view::Col::fixed("pool", 4),
                    view::Col::fixed("action", 7),
                    view::Col::fixed("amount", 14),
                    // Dollars per token, like every other price on screen.
                    // The pool's own tokens-per-ETH is the right number in the
                    // wrong unit: nothing else on the row is quoted that way,
                    // so it could not be compared with anything.
                    view::Col::fixed("price", 16),
                    view::Col::fixed("pooled", 12),
                    view::Col::fixed("mkt cap $", 12),
                    view::Col::fixed("trader", 14),
                    view::Col::min("tx", 12),
                ],
            );
            t.active_key = Some('t');
            // With no pool there is nothing to stream, so saying trades "stream
            // in as they happen" reads as waiting for something that is never
            // coming. Say what is actually missing, in the order it is needed.
            t.empty_note = if bot.pool.kind.is_empty() {
                if bot.trader.is_zero() {
                    "press [W] to unlock an account, then [f] or [k] to pick a token\nnothing trades until you do both".into()
                } else {
                    "press [f] or [k] to pick a token and start trading".into()
                }
            } else if bot.trader.is_zero() {
                "press [W] to unlock an account before you can trade this pool".into()
            } else {
                "no trades on this pool yet\nthey stream in as they happen".into()
            };
            for s in shown.iter().rev().skip(scroll).take(h) {
                let (lbl, atone) = match s.action {
                    engine::TapeAction::Buy => ("BUY", view::Tone::Good),
                    engine::TapeAction::Sell => ("SELL", view::Tone::Bad),
                    engine::TapeAction::Add => ("ADD", view::Tone::Info),
                    engine::TapeAction::Remove => ("REMOVE", view::Tone::Accent),
                };
                let mine = ours.contains(&s.tx);
                // For LP add/remove, show the tick range in the price column.
                let is_lp = matches!(s.action, engine::TapeAction::Add | engine::TapeAction::Remove);
                let mid = if is_lp {
                    // A range is two prices. Ticks are how the pool stores
                    // them, not how anyone reads them.
                    match (tick_usd(bot, s.tick_lo), tick_usd(bot, s.tick_hi)) {
                        (Some(a), Some(b)) => {
                            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                            format!("{}–{}", view::usd_price(lo), view::usd_price(hi))
                        }
                        // Orientation unknown for this venue: the raw ticks are
                        // still true, and true beats a converted guess.
                        _ => format!("tick [{}, {}]", s.tick_lo, s.tick_hi),
                    }
                } else if s.price > 0.0 && bot.pool.quote_usd > 0.0 {
                    // `price` is tokens per unit of quote, so the price OF a
                    // token is the quote's dollar value divided by it.
                    view::usd_price(bot.pool.quote_usd / s.price)
                } else if s.price > 0.0 {
                    format!("{:.6}", s.price) // no USD rate yet; the raw ratio is all there is
                } else {
                    "—".into()
                };
                let (venue, vtone) = if s.is_v4 { ("v4", view::Tone::Info) } else { ("v3", view::Tone::Accent) };
                // The whole row carries the trade's colour, so a buy reads as
                // one green unit and a sell as one red one. Age stays neutral:
                // it says when, not what.
                t.push_mine(
                    vec![
                        view::Cell::toned(if mine { view::MINE_MARK } else { "" }, view::Tone::Warn),
                        view::Cell::new(age(s.block)),
                        view::Cell::bold(venue, vtone),
                        view::Cell::bold(lbl, atone),
                        view::Cell::new(format!("{:.6}", s.eth)),
                        view::Cell::new(mid),
                        view::Cell::toned(
                            if s.liq_eth > 0.0 { view::sol_compact(s.liq_eth) } else { String::new() },
                            atone,
                        ),
                        // Dollars, matching the Solana tape: pooled ETH is the
                        // exit liquidity and belongs in ETH, but a cap in ETH
                        // says nothing at a glance.
                        // supply/price is in the QUOTE currency, so it converts
                        // at the quote's USD value — not ETH's. Using eth_usd on
                        // a USDG pool inflated every cap by ~1850x.
                        view::Cell::toned(
                            if s.price > 0.0 && bot.token_supply > 0.0 {
                                let mc = bot.token_supply / s.price;
                                let usd = bot.pool.quote_usd;
                                if usd > 0.0 {
                                    view::usd_compact(mc * usd)
                                } else {
                                    format!("{mc:.3} {}", bot.pool.quote_sym)
                                }
                            } else {
                                String::new()
                            },
                            atone,
                        ),
                        view::Cell::toned(short_addr(s.trader), view::Tone::Normal),
                        view::Cell::toned(format!("{}", s.tx), view::Tone::Normal),
                    ],
                    mine,
                );
            }
            ui::widgets::table(f, mid_area, &t, None);
        }
        Panel::Orders => {
        // Orders queue as an aligned table: STATUS | ACTION | SIDE | AMOUNT | PRICE | TX.
        let h = mid_area.height.saturating_sub(3).max(1) as usize; // minus borders + header
        // Every order this bot ever sent for the open token, not just this
        // session's — the file holds them all, and "have I traded this coin
        // before?" is exactly the question you ask when a coin is in front of
        // you. Falls back to everything when no token is loaded, or on `o`.
        let filtered = !orders_all && !bot.pool.token.is_zero();
        let rows: Vec<&engine::Order> = bot
            .orders
            .iter()
            .filter(|o| !filtered || o.token == bot.pool.token)
            .collect();
        let total = rows.len();
        let scroll = orders_scroll.min(total.saturating_sub(1));
        // Built as a chain-agnostic TableView and drawn by the shared widget —
        // same code path as the Solana orders panel.
        // "My Orders", not "Orders": this panel is YOUR sends, while the tape
        // beside it is everyone's. One word saves reading two panels to work
        // out which is which.
        // Say WHICH orders these are. A filtered list that looks unfiltered is
        // how you conclude you never traded a coin you traded twice.
        let scope = if filtered {
            // Not "DGMA only": the symbol is in every row and in the header
            // above. What the title has to carry is the way OUT of the filter.
            " [o] all".to_string()
        } else if bot.orders.len() > total {
            " · all tokens".to_string()
        } else {
            String::new()
        };
        let title = if total > h {
            format!(
                " My Orders {}–{} of {}{} ↑/↓ scroll ",
                scroll + 1,
                (scroll + h).min(total),
                total,
                scope
            )
        } else {
            format!(" My Orders ({total}){scope} ")
        };
        let mut t = view::TableView::new(
            title,
            vec![
                // WHEN, first. This list is the record of what this bot sent,
                // and the first question anyone asks of it is "was that me,
                // just now?" — which is a time question.
                //
                // Asked as "how long ago", so answered that way. A clock time
                // has to be subtracted from now before it means anything, and
                // the moment you actually need this column is the moment you
                // are least willing to do arithmetic. The wall clock is one
                // column over, for when the question is "was I at the keyboard
                // at 03:42".
                // One glyph for "this row no longer matches its proof".
                // Blank when it does — a badge on every row teaches you to
                // stop reading it; the only one worth noticing is the broken
                // one. A blank two-character gutter is not a column you read,
                // so `status` is still the first thing your eye lands on.
                view::Col::fixed("", 2),
                // Status first, then WHEN. That is the order the questions
                // come in: did it go through, and was that me just now.
                view::Col::fixed("status", 9),
                // "How long ago" only. A wall clock beside it was two columns
                // answering one question — and the one you actually ask of a
                // trade you are looking at now is how long ago, not what the
                // clock read. The exact second is still on the record.
                view::Col::fixed("ago", 5),
                view::Col::fixed("pool", 4),
                // WHICH key caused it, immediately before what it did — the
                // two halves of one sentence, read together.
                view::Col::fixed("key", 4),
                // Order labels run to "SELL ALL" and "CLOSE ALL"; the longer
                // "REMOVE LP #505" gives up its tail rather than making every
                // BUY row carry four columns of empty space for it.
                view::Col::fixed("action", 10),
                // The ticker it wore WHEN YOU TRADED IT, stored on the order
                // rather than looked up — a scam can rename itself afterwards.
                view::Col::fixed("symbol", 12),
                view::Col::fixed("amount ETH", 13),
                // What the press cost to send. Asked of a single transaction,
                // so answered on the transaction — a buy has no closed trade to
                // hang it on until the sell, which may be days away or never.
                view::Col::fixed("gas $", 8),
                view::Col::fixed("pooled ETH", 11),
                view::Col::fixed("mkt cap", 11),
                view::Col::min("tx", 66),
            ],
        );
        t.empty_note = "no orders yet\npress b to buy  ·  s to sell  ·  a add LP  ·  x sell all".into();
        t.active_key = Some('o');
        for o in rows.iter().rev().skip(scroll).take(h) {
            let (st, stone) = match o.status {
                engine::OrderStatus::Confirmed => ("confirmed", view::Tone::Good),
                engine::OrderStatus::Pending => ("pending", view::Tone::Warn),
                engine::OrderStatus::Reverted => ("reverted", view::Tone::Bad),
                engine::OrderStatus::Failed => ("failed", view::Tone::Bad),
                engine::OrderStatus::Skipped => ("skipped", view::Tone::Dim),
            };
            let (action, _amount, _price) = parse_order(&o.label);
            let atone = match action.to_ascii_uppercase().as_str() {
                s if s.starts_with("BUY") => view::Tone::Good,
                s if s.starts_with("SELL") => view::Tone::Bad,
                s if s.starts_with("ADD") => view::Tone::Info,
                s if s.starts_with("REMOVE") || s.starts_with("CLOSE") => view::Tone::Accent,
                _ => view::Tone::Normal,
            };
            let (venue, vtone) = if o.is_v4 { ("v4", view::Tone::Info) } else { ("v3", view::Tone::Accent) };
            // How long ago, which is the form the question is asked in.
            let ago = if o.at > 0 {
                view::age_compact(crate::ledger::now().saturating_sub(o.at) as f64)
            } else {
                "—".to_string()
            };
            t.push(vec![
                if o.verified || o.proof.is_empty() && o.at == 0 {
                    // Verified, or too old to have a proof at all. Either way
                    // there is nothing to shout about.
                    view::Cell::new("")
                } else {
                    view::Cell::bold("⚠", view::Tone::Bad)
                },
                view::Cell::bold(st, stone),
                view::Cell::toned(ago, view::Tone::Normal),
                view::Cell::bold(venue, vtone),
                view::Cell::toned(
                    if o.key.is_empty() { "—".to_string() } else { o.key.clone() },
                    view::Tone::Info,
                ),
                view::Cell::bold(action, atone),
                // `$` prefixed, the way a ticker is written everywhere else —
                // and the way it is spoken, which is what you compare against
                // when you are checking a coin you half remember.
                view::Cell::bold(
                    if o.sym.is_empty() { "—".to_string() } else { format!("${}", o.sym) },
                    view::Tone::Accent,
                ),
                // Amount always in ETH numeraire (cost in ether).
                view::Cell::new(if o.eth > 0.0 { format!("{:.6}", o.eth) } else { String::new() }),
                view::Cell::toned(
                    if o.gas > 0.0 && bot.eth_usd > 0.0 {
                        format!("{:.3}", o.gas * bot.eth_usd)
                    } else if o.gas > 0.0 {
                        view::eth(o.gas)
                    } else {
                        // Zero is the answer on a chain whose gas rounds to
                        // nothing, and also what an order written before gas
                        // was measured knows. Blank rather than a bold "0.000".
                        String::new()
                    },
                    view::Tone::Dim,
                ),
                view::Cell::new(if o.pooled > 0.0 { view::eth(o.pooled) } else { String::new() }),
                view::Cell::new(if o.mc > 0.0 {
                    if bot.eth_usd > 0.0 {
                        view::usd_compact(o.mc * bot.eth_usd)
                    } else {
                        format!("{} ETH", view::eth(o.mc))
                    }
                } else {
                    String::new()
                }),
                view::Cell::toned(
                    o.hash.map(|h| format!("{h:#x}")).unwrap_or_default(),
                    view::Tone::Normal,
                ),
            ]);
        }
        ui::widgets::table(f, mid_area, &t, None);
        }
    }

    // Trimmed footer — essentials only; '?' opens the full shortcut list.
    let mut keys: Vec<Span> = vec![
        Span::styled("[b]", Style::default().fg(ui::widgets::tone_color(view::Tone::Good)).add_modifier(Modifier::BOLD)),
        Span::raw(" buy  "),
        Span::styled("[s]", Style::default().fg(ui::widgets::tone_color(view::Tone::Bad)).add_modifier(Modifier::BOLD)),
        Span::raw(" sell  "),
        Span::styled("[x]", Style::default().fg(ui::widgets::tone_color(view::Tone::Warn)).add_modifier(Modifier::BOLD)),
        Span::raw(" sell-all/close  "),
        Span::styled("[f]", Style::default().fg(ui::widgets::tone_color(view::Tone::Accent)).add_modifier(Modifier::BOLD)),
        Span::raw(" find  "),
        Span::styled("[?]", Style::default().fg(ui::widgets::border_color()).add_modifier(Modifier::BOLD)),
        Span::raw(" help  "),

        Span::styled("[W]", Style::default().fg(ui::widgets::border_color()).add_modifier(Modifier::BOLD)),
        Span::raw(" wallet  "),
        Span::styled("[D]", Style::default().fg(ui::widgets::border_color()).add_modifier(Modifier::BOLD)),
        Span::raw(" docs  "),
        Span::styled("[q]", Style::default().fg(ui::widgets::border_color()).add_modifier(Modifier::BOLD)),
        Span::raw(" quit"),
    ];

    // Only when there is one. A key advertising an update that does not exist
    // is a key that trains you to ignore it — so when there is nothing to
    // install the footer says so on the right instead, rather than here.
    if let Some(v) = update::available() {
        keys.push(Span::styled(
            "  [U]",
            Style::default().fg(ui::widgets::tone_color(view::Tone::Info)).add_modifier(Modifier::BOLD),
        ));
        keys.push(Span::styled(
            format!(" update to {v}"),
            Style::default().fg(ui::widgets::tone_color(view::Tone::Info)),
        ));
    }

    // The build, against the right edge. Dim, because it is not something to
    // read — it is something to quote when a bug report needs to name what was
    // running, and it should be on screen without asking.
    let build = update::footer_label();
    let used: usize = keys.iter().map(|s| s.content.chars().count()).sum();
    let inner = foot_area.width.saturating_sub(2) as usize;
    // Dropped rather than wrapped when the terminal is narrow: the keys are
    // what the footer is for.
    if inner > used + build.chars().count() + 2 {
        keys.push(Span::raw(" ".repeat(inner - used - build.chars().count())));
        keys.push(Span::styled(
            build,
            Style::default().fg(ui::widgets::tone_color(view::Tone::Dim)),
        ));
    }
    let footer = Paragraph::new(Line::from(keys)).block(ui::widgets::themed_block(""));
    f.render_widget(footer, foot_area);

    // Full shortcut list overlay ('?') — a grouped two-column list: key on the
    // left, description on the right. Related knobs ( [ ] ( ) { } ) grouped.
    if show_help {
        // (section, key, description). Empty key = section header.
        let items: [(&str, &str); 35] = [
            ("TRADE", ""),
            ("", "b|buy"),
            ("", "s|sell"),
            ("", "x|sell all"),
            ("", "h|token holdings"),
            ("LIQUIDITY", ""),
            ("", "a|add liquidity"),
            ("", "r|remove last liquidity"),
            ("DISCOVER", ""),
            ("", "f|find market"),
            ("", "F|verified tokens"),
            ("", "k|leaderboard"),
            ("POOL / ARB", ""),
            ("", "p|select pool"),
            ("", "Del|deselect the pool"),
            ("", "d|toggle multi-pool view"),
            ("", "e|auto arbitrage"),
            ("VIEW", ""),
            ("", "t  o  l  c|trades · orders · logs · chart"),
            ("", "O  → ←|cycle panels"),
            ("", "↑ ↓|scroll"),
            ("", "c  v|candlestick chart"),
            ("", ",  .|candle interval −/+"),
            ("", "L|PnL calendar"),
            ("SIZE", ""),
            ("", "[  ]|buy size −/+"),
            ("", ";  '|buy step finer/coarser"),
            ("", "(  )|sell size −/+"),
            ("", "{  }  0|slippage −/+"),
            ("MODE", ""),
            ("", "T|theme picker"),
            ("", "g|toggle profit guard"),
            ("", "n|toggle buy dedup"),
            ("", "q|quit"),
            ("", "?|help"),
        ];
        // Shared with the Solana dashboard — one renderer, one look.
        ui::widgets::help(f, &items, " Shortcuts  (any key to close) ");
    }
    Some(logo_box)
}

/// Ask for a v4 fee tier; returns (fee, tickSpacing).
fn fee_tier_select(
    term: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
) -> eyre::Result<Option<(u32, i32)>> {
    let opts = vec![
        "0.05%   (tickSpacing 10)".to_string(),
        "0.30%   (tickSpacing 60)".to_string(),
        "1%      (tickSpacing 200)".to_string(),
    ];
    Ok(match ui::select(term, "Fee tier", &opts)? {
        Some(0) => Some((500, 10)),
        Some(1) => Some((3000, 60)),
        Some(2) => Some((10000, 200)),
        _ => None,
    })
}

/// price (token per ETH) -> sqrtPriceX96 for pool initialization.
fn price_to_sqrtx96(price: f64) -> alloy::primitives::aliases::U160 {
    use std::str::FromStr;
    type U160 = alloy::primitives::aliases::U160;
    let x = price.max(1e-18).sqrt() * 2f64.powi(96);
    U160::from_str(&format!("{x:.0}")).unwrap_or_else(|_| U160::from(1u8) << 96) // ~price 1.0
}

fn wei_f64(x: alloy::primitives::U256) -> f64 {
    x.to_string().parse::<f64>().unwrap_or(0.0) / 1e18
}

/// Result of the two-asset wallet picker.
enum Pick {
    Token(alloy::primitives::Address, String),
    NeedsEth,
    Cancelled,
}

/// Let the user pick a pair from their wallet assets (ETH + known tokens, with
/// live balances). ETH is currency0; returns the non-ETH token of the pair.
async fn pick_pair<P: Provider>(
    term: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    provider: &P,
    assets: &[(alloy::primitives::Address, String)],
    trader: alloy::primitives::Address,
) -> eyre::Result<Pick> {
    use alloy::primitives::Address;
    let mut labels = Vec::new();
    for (addr, sym) in assets {
        let bal = if *addr == Address::ZERO {
            provider.get_balance(trader).await.map(wei_f64).unwrap_or(0.0)
        } else {
            contracts::IERC20::new(*addr, provider)
                .balanceOf(trader)
                .call()
                .await
                .map(|b| wei_f64(b._0))
                .unwrap_or(0.0)
        };
        labels.push(format!("{:<8} balance {:.6}", sym, bal));
    }
    let picked = match ui::multi_select(term, "Tick the two assets to pair", &labels, 2)? {
        Some(v) => v,
        None => return Ok(Pick::Cancelled),
    };
    if picked.len() != 2 {
        return Ok(Pick::NeedsEth); // must tick exactly two (one being ETH)
    }
    let aa = assets[picked[0]].0;
    let bb = assets[picked[1]].0;
    if aa == Address::ZERO && bb != Address::ZERO {
        Ok(Pick::Token(bb, assets[picked[1]].1.clone()))
    } else if bb == Address::ZERO && aa != Address::ZERO {
        Ok(Pick::Token(aa, assets[picked[0]].1.clone()))
    } else {
        Ok(Pick::NeedsEth)
    }
}

/// Split an order label into (action, amount, price) columns for the table.
/// Handles trades ("BUY ~0.00017 ETH @ 1.0"), skips ("BUY (no edge +0.0001)"),
/// LP ops ("ADD LP ~0.001 ETH", "REMOVE LP #505"), and pool ops.
fn parse_order(label: &str) -> (String, String, String) {
    // Trade with a price.
    if let Some((lhs, price)) = label.split_once(" @ ") {
        let mut it = lhs.splitn(2, ' ');
        let action = it.next().unwrap_or("").to_string();
        let amount = it.next().unwrap_or("").trim_start_matches('~').to_string();
        // The label carries "[v3 0.3%] liq_eth=…" after the price for the log
        // line's benefit. The table already has dedicated pool / pooled columns,
        // so cut it here rather than repeating it inside the price cell.
        let price = price.split(" [").next().unwrap_or(price).trim().to_string();
        return (action, amount, price);
    }
    // Skipped trade with an edge note: "BUY (no edge +0.0001)".
    if let Some((side, rest)) = label.split_once(" (") {
        return (side.to_string(), rest.trim_end_matches(')').to_string(), String::new());
    }
    // Multi-word LP / pool actions.
    for pref in ["ADD LP", "REMOVE LP", "CLOSE LP", "CLOSE ALL", "CREATE POOL"] {
        if let Some(rest) = label.strip_prefix(pref) {
            return (pref.to_string(), rest.trim().trim_start_matches('~').to_string(), String::new());
        }
    }
    (label.to_string(), String::new(), String::new())
}

/// Fee tier as a human label: 10000 -> "1%", 3000 -> "0.3%", 500 -> "0.05%".
fn fee_label(fee: u32) -> String {
    let pct = fee as f64 / 10_000.0;
    let s = format!("{pct:.4}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("{s}%")
}

// ---------------- interactive selection (arrow-key menus via inquire) -------

