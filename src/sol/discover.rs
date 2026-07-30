// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! pump.fun launch discovery — the Solana "trenches".
//!
//! Mirrors the EVM discovery flow: scan for launches, fetch each one's live
//! metrics, publish rows as they arrive. Launches come from decoding `create` /
//! `create_v2` instructions on the pump program, which conveniently carry the
//! coin's name and symbol inline — no metadata fetch needed.
//!
//! ⚠️ Coin names and symbols are attacker-controlled strings from chain data.
//! They are sanitised (`clean_text`) before display and are never interpolated
//! into shell commands, prompts, or file paths.

use solana_pubkey::Pubkey;

use super::pumpfun::BondingCurve;
use super::rpc::Rpc;
use super::{bonding_curve_pda, PUMP_PROGRAM};
use crate::view::{self, Cell, Col, TableView, Tone};

/// Instruction discriminators that mint a new coin.
const DISC_CREATE: [u8; 8] = [24, 30, 200, 40, 5, 28, 7, 119];
const DISC_CREATE_V2: [u8; 8] = [214, 144, 76, 236, 95, 139, 49, 180];

/// A freshly-created coin, straight off a `create` instruction.
#[derive(Debug, Clone)]
pub struct Launch {
    pub mint: Pubkey,
    pub name: String,
    pub symbol: String,
    pub signature: String,
    /// Unix seconds from the transaction's `blockTime` — the authoritative
    /// launch moment. `None` when the node omits it (very recent slots).
    pub block_time: Option<i64>,
}

/// A launch plus its live curve metrics — one row of the trenches table.
#[derive(Debug, Clone)]
pub struct TrenchRow {
    pub launch: Launch,
    pub curve: BondingCurve,
}

impl TrenchRow {
    pub fn mkt_cap_sol(&self) -> f64 {
        self.curve.market_cap_sol()
    }
    pub fn pooled_sol(&self) -> f64 {
        self.curve.pooled_sol()
    }
    pub fn progress(&self) -> f64 {
        self.curve.progress()
    }

    /// Seconds since the coin launched, from the block time.
    pub fn age_secs(&self) -> Option<f64> {
        let bt = self.launch.block_time?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs() as i64;
        Some((now - bt).max(0) as f64)
    }

    /// Compact age for the table, or `—` when the node gave no block time.
    pub fn age_text(&self) -> String {
        self.age_secs().map(crate::view::age_compact).unwrap_or_else(|| "—".into())
    }
}

/// Strip anything that could corrupt a terminal or smuggle content out of a
/// cell: control characters, newlines, and ANSI escapes. Truncates hard.
///
/// On-chain text is untrusted input — a coin can be named with escape sequences
/// or an instruction-shaped string, so it is neutered before it reaches the UI.
pub fn clean_text(s: &str, max: usize) -> String {
    let cleaned: String = s
        .chars()
        .filter(|c| !c.is_control() && *c != '\u{1b}')
        .take(max)
        .collect();
    let t = cleaned.trim();
    if t.is_empty() { "?".to_string() } else { t.to_string() }
}

/// Read a borsh-encoded `string` (u32 LE length + utf8 bytes) at `off`.
/// Returns the string and the offset just past it.
fn borsh_string(data: &[u8], off: usize) -> Option<(String, usize)> {
    if data.len() < off + 4 {
        return None;
    }
    let len = u32::from_le_bytes(data[off..off + 4].try_into().ok()?) as usize;
    let start = off + 4;
    let end = start.checked_add(len)?;
    // Guard against a hostile length field pointing past the buffer.
    if len > 512 || data.len() < end {
        return None;
    }
    Some((String::from_utf8_lossy(&data[start..end]).to_string(), end))
}

/// Decode the `name` and `symbol` args of a `create` / `create_v2` instruction.
/// Both start with `name: string, symbol: string`, so one decoder serves each.
fn decode_create_args(data: &[u8]) -> Option<(String, String)> {
    if data.len() < 8 {
        return None;
    }
    let disc: [u8; 8] = data[..8].try_into().ok()?;
    if disc != DISC_CREATE && disc != DISC_CREATE_V2 {
        return None;
    }
    let (name, off) = borsh_string(data, 8)?;
    let (symbol, _) = borsh_string(data, off)?;
    Some((clean_text(&name, 32), clean_text(&symbol, 12)))
}

/// Base58 decode (transaction instruction data arrives base58-encoded in the
/// `json` transaction encoding).
fn b58_decode(s: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    let mut out: Vec<u8> = Vec::new();
    for ch in s.bytes() {
        let mut carry = ALPHABET.iter().position(|&a| a == ch)?;
        for byte in out.iter_mut() {
            carry += 58 * (*byte as usize);
            *byte = (carry & 0xff) as u8;
            carry >>= 8;
        }
        while carry > 0 {
            out.push((carry & 0xff) as u8);
            carry >>= 8;
        }
    }
    // Leading '1's are leading zero bytes.
    for _ in s.bytes().take_while(|&c| c == b'1') {
        out.push(0);
    }
    out.reverse();
    Some(out)
}

/// Scan the most recent pump-program transactions for new coin launches.
///
/// `limit` bounds how many signatures are inspected — each one costs a
/// `getTransaction`, so this is the main cost knob.
pub async fn scan_launches(rpc: &Rpc, limit: u32) -> Vec<Launch> {
    let sigs = match rpc.signatures_for(&PUMP_PROGRAM, limit).await {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for sig in sigs {
        out.extend(launches_in_tx(rpc, &sig).await);
    }
    out
}

// ---- live launch feed (WebSocket) ----------------------------------------
//
// Polling `getSignaturesForAddress` does NOT work for launch discovery:
// pump.fun's signature stream is overwhelmingly trades, so a 25-signature poll
// routinely contains zero `create` instructions (verified against mainnet).
// `logsSubscribe` is push-based — every create arrives the moment it lands, with
// no polling cost and nothing missed between polls.


/// Full account-key list for a transaction, in the order instruction indices
/// address them.
///
/// **Versioned (v0) transactions use Address Lookup Tables**, so
/// `message.accountKeys` holds only the STATIC keys — indices beyond that resolve
/// into `meta.loadedAddresses`, writable first then readonly. Reading only the
/// static list silently drops every ALT-routed instruction, which is most pump
/// traffic (aggregators all use lookup tables). That made the trades view look
/// almost empty.
fn all_account_keys(tx: &serde_json::Value) -> Vec<String> {
    let mut keys: Vec<String> = tx
        .get("transaction")
        .and_then(|t| t.get("message"))
        .and_then(|m| m.get("accountKeys"))
        .and_then(|k| k.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).map(str::to_string).collect())
        .unwrap_or_default();
    if let Some(loaded) = tx.get("meta").and_then(|m| m.get("loadedAddresses")) {
        for field in ["writable", "readonly"] {
            if let Some(arr) = loaded.get(field).and_then(|w| w.as_array()) {
                keys.extend(arr.iter().filter_map(|v| v.as_str()).map(str::to_string));
            }
        }
    }
    keys
}

/// Turn an HTTP RPC URL into its WebSocket equivalent. Solana nodes conventionally
/// serve WS on the same host: `https://x` → `wss://x`, `http://x` → `ws://x`.
pub fn ws_url_from_http(http: &str) -> String {
    let ws = if let Some(rest) = http.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = http.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        http.to_string()
    };
    normalize_ws_url(&ws)
}

/// Ensure the URL has a path before its query string.
///
/// `wss://host?key=x` has an EMPTY path, so the handshake goes out as
/// `GET ?key=x HTTP/1.1` — not a valid request-target, and providers answer
/// 400. Browsers and curl paper over this by inserting `/`; tungstenite sends
/// what it's given. Providers hand out exactly this shape (Flux's dashboard
/// shows `wss://ws.us.fluxrpc.com?key=...`), so normalise rather than expecting
/// the config to be written defensively.
pub fn normalize_ws_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    match rest.split_once('?') {
        // Authority contains no '/', so the path is empty — insert one.
        Some((authority, query)) if !authority.contains('/') => {
            format!("{scheme}://{authority}/?{query}")
        }
        _ => url.to_string(),
    }
}

/// Extract a mint from a `create` log group. Solana log lines don't carry the
/// account list, so the mint is recovered from the transaction referenced by the
/// notification; this returns the signature for that follow-up fetch.
fn create_sig_from_logs(v: &serde_json::Value) -> Option<String> {
    let result = v.get("params")?.get("result")?;
    let value = result.get("value")?;
    // Skip failed transactions — a reverted create never produced a coin.
    if !value.get("err").map(|e| e.is_null()).unwrap_or(false) {
        return None;
    }
    let logs = value.get("logs")?.as_array()?;
    // Anchor emits "Program log: Instruction: Create" for create/create_v2.
    let is_create = logs.iter().filter_map(|l| l.as_str()).any(|l| {
        let l = l.trim();
        l == "Program log: Instruction: Create" || l == "Program log: Instruction: CreateV2"
    });
    if !is_create {
        return None;
    }
    value.get("signature")?.as_str().map(str::to_string)
}

/// Subscribe to pump.fun launches and hand each one to `on_launch` as it lands.
///
/// Runs until the socket drops or the callback returns `false`. The caller owns
/// reconnection policy — this deliberately doesn't loop forever on its own so a
/// dead endpoint surfaces rather than silently spinning.
/// Watch launches across several websocket endpoints, reconnecting on drop.
///
/// A single `watch_launches` call ends the moment its socket closes — and
/// sockets close routinely (provider restarts, idle timeouts, rate limits).
/// Without this, the trenches went permanently blank after the first drop and
/// the screen still read "watching…". Endpoints rotate on failure so one
/// provider's outage doesn't stop the feed.
///
/// Returns only when `on_launch` asks to stop; otherwise it retries forever
/// with a capped backoff.
pub async fn watch_launches_ha<F, S>(
    rpc: &Rpc,
    urls: &[String],
    mut on_launch: F,
    mut on_status: S,
) -> eyre::Result<()>
where
    F: FnMut(Launch) -> bool,
    S: FnMut(String) + Clone,
{
    if urls.is_empty() {
        eyre::bail!("no websocket endpoint configured");
    }
    let mut attempt: usize = 0;
    let mut stop = false;
    loop {
        let url = &urls[attempt % urls.len()];
        let safe = url.split('?').next().unwrap_or(url).to_string();
        let r = watch_launches(
            rpc,
            url,
            |l| {
                let go = on_launch(l);
                if !go {
                    stop = true;
                }
                go
            },
            on_status.clone(),
        )
        .await;
        if stop {
            return Ok(());
        }
        attempt += 1;
        // 1s, 2s, 4s … capped at 15s. Fast enough to recover from a blip,
        // slow enough not to hammer a provider that is rejecting us.
        let backoff = std::time::Duration::from_secs((1u64 << (attempt.min(4))).min(15));
        match r {
            Ok(()) => on_status(format!("{safe} closed the feed; reconnecting in {}s", backoff.as_secs())),
            Err(e) => on_status(format!("{safe} failed ({e}); retrying in {}s", backoff.as_secs())),
        }
        tokio::time::sleep(backoff).await;
    }
}

pub async fn watch_launches<F, S>(
    rpc: &Rpc,
    ws_url: &str,
    mut on_launch: F,
    mut on_status: S,
) -> eyre::Result<()>
where
    F: FnMut(Launch) -> bool,
    S: FnMut(String),
{
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    // Redact the key before it can reach a log line or the UI.
    let safe_url = ws_url.split('?').next().unwrap_or(ws_url).to_string();
    let ws_url = &normalize_ws_url(ws_url);
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url.as_str())
        .await
        .map_err(|e| eyre::eyre!("websocket connect to {safe_url} failed: {e}"))?;
    on_status(format!("connected to {safe_url}, subscribing…"));

    // Only pump-program logs, and only confirmed ones.
    let sub = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "logsSubscribe",
        // `processed`: the earliest the chain will whisper a create — this is
        // where pump.fun's own site lives, and `confirmed` gave it a full
        // optimistic-confirmation head start on every launch. The tx fetch
        // below retries the moment gap away.
        "params": [{"mentions": [PUMP_PROGRAM.to_string()]}, {"commitment": "processed"}]
    });
    ws.send(Message::Text(sub.to_string())).await?;

    // Trace throughput: a quiet feed and a broken feed look identical on screen
    // otherwise, which is exactly the case that wasted time here.
    let mut msgs: u64 = 0;
    let mut creates: u64 = 0;
    let mut last_report = std::time::Instant::now();

    while let Some(msg) = ws.next().await {
        let text = match msg {
            Ok(Message::Text(t)) => t,
            Ok(Message::Ping(p)) => {
                let _ = ws.send(Message::Pong(p)).await;
                continue;
            }
            Ok(Message::Close(_)) | Err(_) => break,
            _ => continue,
        };
        msgs += 1;
        if last_report.elapsed() >= std::time::Duration::from_secs(10) {
            on_status(format!("feed alive: {msgs} msgs, {creates} launches"));
            last_report = std::time::Instant::now();
        }
        let v: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // The first reply is the subscription ack, not a notification.
        if v.get("result").is_some() && v.get("params").is_none() {
            on_status("subscribed — waiting for launches".to_string());
            continue;
        }
        let sig = match create_sig_from_logs(&v) {
            Some(s) => s,
            None => continue,
        };
        creates += 1;
        // Logs identify the transaction; the mint + name come from decoding it.
        for launch in launches_in_tx(rpc, &sig).await {
            if !on_launch(launch) {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Decode every `create` instruction in one transaction into a `Launch`.
/// Shared by the polling scan and the live feed.
pub async fn launches_in_tx(rpc: &Rpc, sig: &str) -> Vec<Launch> {
    // The subscription hears the create at `processed`; getTransaction only
    // answers once the tx reaches `confirmed`, a moment later. A few short
    // retries bridge exactly that gap — without them, subscribing earlier
    // would just trade latency for missed launches.
    for wait_ms in [0u64, 250, 400, 600, 900, 1300] {
        if wait_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
        }
        // A tx that has not reached `confirmed` answers `result: null` — a
        // SUCCESS to the transport, nothing to the decoder. Only a non-null
        // answer ends the ladder; treating null as "done" silently dropped
        // 26 of 30 launches in a live measurement, because only creates that
        // happened to confirm before the first fetch ever decoded.
        if let Ok(tx) = rpc.transaction(sig).await {
            if !tx.is_null() {
                return decode_launches(&tx, sig);
            }
        }
    }
    Vec::new()
}

/// Pure decoder: pull `create` launches out of a `getTransaction` response.
/// Split out from I/O so it can be tested against a recorded transaction.
pub fn decode_launches(tx: &serde_json::Value, sig: &str) -> Vec<Launch> {
    // Launch time comes from the block, not from when we happened to see it, so
    // ages stay correct for coins discovered by a catch-up scan too.
    let block_time = tx.get("blockTime").and_then(|b| b.as_i64());
    let msg = match tx.get("transaction").and_then(|t| t.get("message")) {
        Some(m) => m,
        None => return Vec::new(),
    };
    let keys = all_account_keys(tx);
    let pump_str = PUMP_PROGRAM.to_string();
    let mut out = Vec::new();

    // Top-level AND inner instructions: a create can be CPI'd by another program.
    let mut candidates: Vec<&serde_json::Value> =
        msg.get("instructions").and_then(|i| i.as_array()).map(|a| a.iter().collect()).unwrap_or_default();
    if let Some(groups) = tx.get("meta").and_then(|m| m.get("innerInstructions")).and_then(|g| g.as_array()) {
        for g in groups {
            if let Some(arr) = g.get("instructions").and_then(|i| i.as_array()) {
                candidates.extend(arr.iter());
            }
        }
    }

    for ix in candidates {
        let prog_idx = ix.get("programIdIndex").and_then(|p| p.as_u64()).unwrap_or(u64::MAX) as usize;
        if keys.get(prog_idx).map(String::as_str) != Some(pump_str.as_str()) {
            continue;
        }
        let data = match ix.get("data").and_then(|d| d.as_str()).and_then(b58_decode) {
            Some(d) => d,
            None => continue,
        };
        let (name, symbol) = match decode_create_args(&data) {
            Some(v) => v,
            None => continue,
        };
        // Account #0 of both create variants is the new mint.
        if let Some(mint) = ix
            .get("accounts")
            .and_then(|a| a.as_array())
            .and_then(|a| a.first())
            .and_then(|i| i.as_u64())
            .and_then(|i| keys.get(i as usize))
            .and_then(|s| s.parse::<Pubkey>().ok())
        {
            out.push(Launch { mint, name, symbol, signature: sig.to_string(), block_time });
        }
    }
    out
}

/// Fetch each launch's bonding curve and keep the ones still on the curve.
/// Graduated coins are dropped — they trade on the AMM, which this path can't
/// execute against, so listing them would offer a trade we can't make.
pub async fn enrich(rpc: &Rpc, launches: Vec<Launch>) -> Vec<TrenchRow> {
    let mut rows = Vec::new();
    for launch in launches {
        let key = bonding_curve_pda(&launch.mint);
        if let Ok(Some((data, _))) = rpc.account(&key).await {
            if let Ok(curve) = BondingCurve::decode(&data) {
                if !curve.complete {
                    rows.push(TrenchRow { launch, curve });
                }
            }
        }
    }
    sort_newest_first(&mut rows);
    rows
}

/// Newest launch at the top.
///
/// Ranking by market cap buried brand-new coins at the bottom — exactly the
/// ones the trenches exist to catch, since every coin starts at the same
/// virtual-reserve cap and only earns a bigger one by surviving. Mint breaks
/// ties so same-second launches hold a fixed order instead of swapping on
/// every redraw.
pub fn sort_newest_first(rows: &mut [TrenchRow]) {
    rows.sort_by(|a, b| {
        b.launch
            .block_time
            .unwrap_or(i64::MIN)
            .cmp(&a.launch.block_time.unwrap_or(i64::MIN))
            .then_with(|| a.launch.mint.cmp(&b.launch.mint))
    });
}

/// How many trades the tape keeps. Deep enough to read a coin's whole early
/// history, bounded so a long session can't grow without limit.
pub const TAPE_RING: usize = 500;

/// Fold a freshly-fetched window of trades into the tape already on screen.
///
/// Each poll only sees the most recent handful of signatures, so assigning that
/// window straight to the tape threw away everything older — trades scrolled
/// off after seconds and the history never accumulated. Worse, the RPC pool
/// round-robins across providers whose nodes are a moment out of step, so
/// consecutive polls disagree about the newest trades and rows flickered in and
/// out.
///
/// Merging by signature fixes both: history accumulates, and a trade one
/// provider hasn't caught up on yet stays put instead of vanishing.
///
/// Order is newest-first and fully deterministic — ties broken by signature so
/// same-block trades never swap places between frames.
///
/// Returns the rows that were genuinely new, oldest first, so the caller can
/// narrate them into the log exactly once.
pub fn merge_tape(tape: &mut Vec<SolSwap>, fresh: Vec<SolSwap>) -> Vec<SolSwap> {
    use std::collections::HashSet;
    // Identity is (signature, event index) — NOT the signature alone. One
    // transaction legitimately carries several trades on the same pool, and
    // collapsing them to one leg hid every arbitrage sell.
    let mut seen: HashSet<(String, usize)> =
        tape.iter().map(|t| (t.signature.clone(), t.event_idx)).collect();
    let add: Vec<SolSwap> = fresh
        .into_iter()
        .filter(|t| seen.insert((t.signature.clone(), t.event_idx)))
        .collect();
    if add.is_empty() {
        return Vec::new();
    }
    // Oldest first: the log reads chronologically, the table newest-first.
    let mut added = add.clone();
    added.sort_by(|a, b| {
        (a.slot, a.block_time.unwrap_or(i64::MIN), &a.signature, a.event_idx)
            .cmp(&(b.slot, b.block_time.unwrap_or(i64::MIN), &b.signature, b.event_idx))
    });
    tape.extend(add);
    // Slot first — the chain's own order. Time only covers a missing slot,
    // and the signature only keeps the sort stable within one slot.
    tape.sort_by(|a, b| {
        (b.slot, b.block_time.unwrap_or(i64::MIN))
            .cmp(&(a.slot, a.block_time.unwrap_or(i64::MIN)))
            .then_with(|| a.signature.cmp(&b.signature))
            .then_with(|| a.event_idx.cmp(&b.event_idx))
    });
    tape.truncate(TAPE_RING);
    added
}

/// Render trench rows into the shared, chain-agnostic table model — the same
/// `TableView` the EVM screens use, so both chains render through one widget.
pub fn table_view(
    rows: &[TrenchRow],
    sol_usd: f64,
    risk: Option<&super::rugcheck::RugCheck>,
    warn_score: u32,
) -> TableView {
    let mut t = TableView::new(
        format!(" Trenches — {} live coins   j/k select · Enter trade · Esc back ", rows.len()),
        vec![
            Col::fixed("sym", 12),
            Col::fixed("pooled SOL", 12),
            Col::fixed("mkt cap", 12),
            Col::fixed("bonded", 8),
            Col::fixed("age", 7),
            Col::fixed("risk", 8),
            Col::min("mint", 20),
        ],
    );
    // The screen you sit and wait on, so it carries the light.
    t.health = true;
    t.empty_note = "watching pump.fun for new launches…\nthey appear the moment they are created  ·  esc to go back".into();
    for r in rows {
        let mc = r.mkt_cap_sol();
        // Show USD when we have a SOL price, else stay in SOL — never invent a rate.
        let mc_txt = if sol_usd > 0.0 { view::usd_compact(mc * sol_usd) } else { format!("{mc:.2} SOL") };
        t.push(vec![
            Cell::bold(r.launch.symbol.clone(), Tone::Info),
            Cell::new(view::sol_compact(r.pooled_sol())),
            Cell::new(mc_txt),
            Cell::toned(
                format!("{:.0}%", r.progress() * 100.0),
                if r.progress() > 0.8 { Tone::Good } else { Tone::Normal },
            ),
            Cell::new(r.age_text()),
            // Cached only — never blocks the render on a network call.
            match risk.and_then(|rc| rc.cached(&r.launch.mint.to_string())) {
                Some(rep) => Cell::bold(rep.badge(), rep.tone(warn_score)),
                None => Cell::toned("…", Tone::Dim),
            },
            Cell::toned(r.launch.mint.to_string(), Tone::Normal),
        ]);
    }
    t
}

#[cfg(test)]
mod ws_url_tests {
    use super::{normalize_ws_url, ws_url_from_http};

    #[test]
    fn a_query_without_a_path_gets_one() {
        // The exact shape providers hand out. Without the inserted '/', the
        // handshake request-target is invalid and the server answers 400 —
        // which is precisely how the launch feed died silently.
        assert_eq!(
            normalize_ws_url("wss://ws.us.fluxrpc.com?key=abc"),
            "wss://ws.us.fluxrpc.com/?key=abc"
        );
        // Already valid: left exactly as-is.
        assert_eq!(
            normalize_ws_url("wss://ws.us.fluxrpc.com/?key=abc"),
            "wss://ws.us.fluxrpc.com/?key=abc"
        );
        assert_eq!(
            normalize_ws_url("wss://mainnet.helius-rpc.com/v1/?api-key=abc"),
            "wss://mainnet.helius-rpc.com/v1/?api-key=abc"
        );
        // No query at all, and unparseable input, both pass through.
        assert_eq!(normalize_ws_url("wss://api.mainnet-beta.solana.com"), "wss://api.mainnet-beta.solana.com");
        assert_eq!(normalize_ws_url("garbage"), "garbage");
    }

    #[test]
    fn http_urls_convert_and_normalise_together() {
        assert_eq!(ws_url_from_http("https://us.fluxrpc.com?key=k"), "wss://us.fluxrpc.com/?key=k");
        assert_eq!(ws_url_from_http("http://localhost:8899"), "ws://localhost:8899");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base58_decodes_known_values() {
        // The system program's all-zero address is 32 '1' characters.
        assert_eq!(b58_decode("11111111111111111111111111111111"), Some(vec![0u8; 32]));
        // Round-trip a known pubkey through parse + decode.
        let k = PUMP_PROGRAM.to_string();
        assert_eq!(b58_decode(&k).unwrap().len(), 32);
        assert_eq!(b58_decode(&k).unwrap(), PUMP_PROGRAM.to_bytes().to_vec());
    }

    #[test]
    fn create_args_decode_name_and_symbol() {
        let mut data = Vec::new();
        data.extend_from_slice(&DISC_CREATE);
        data.extend_from_slice(&4u32.to_le_bytes());
        data.extend_from_slice(b"Test");
        data.extend_from_slice(&3u32.to_le_bytes());
        data.extend_from_slice(b"TST");
        assert_eq!(decode_create_args(&data), Some(("Test".into(), "TST".into())));

        // A non-create instruction must be ignored, not misparsed.
        let mut other = vec![1u8; 8];
        other.extend_from_slice(&4u32.to_le_bytes());
        other.extend_from_slice(b"Test");
        assert_eq!(decode_create_args(&other), None);
    }

    /// A hostile length prefix must not panic or over-read.
    #[test]
    fn borsh_string_rejects_bogus_lengths() {
        let mut data = Vec::new();
        data.extend_from_slice(&DISC_CREATE);
        data.extend_from_slice(&u32::MAX.to_le_bytes()); // absurd length
        data.extend_from_slice(b"short");
        assert_eq!(decode_create_args(&data), None, "must reject, not panic");

        // Length that points just past the buffer.
        let mut d2 = Vec::new();
        d2.extend_from_slice(&DISC_CREATE);
        d2.extend_from_slice(&10u32.to_le_bytes());
        d2.extend_from_slice(b"abc");
        assert_eq!(decode_create_args(&d2), None);
    }

    /// Coin names are attacker-controlled — escapes and control chars must never
    /// reach the terminal.
    #[test]
    fn clean_text_neutralises_hostile_names() {
        assert_eq!(clean_text("\u{1b}[31mRED\u{1b}[0m", 32), "[31mRED[0m");
        assert_eq!(clean_text("line\nbreak\r\n", 32), "linebreak");
        assert_eq!(clean_text("", 32), "?");
        assert_eq!(clean_text("   ", 32), "?");
        assert_eq!(clean_text(&"x".repeat(100), 8), "xxxxxxxx", "must truncate");
    }

    #[test]
    fn ws_url_derivation() {
        assert_eq!(ws_url_from_http("https://api.mainnet-beta.solana.com"), "wss://api.mainnet-beta.solana.com");
        assert_eq!(ws_url_from_http("http://127.0.0.1:8899"), "ws://127.0.0.1:8899");
        assert_eq!(ws_url_from_http("wss://already.ws"), "wss://already.ws");
    }

    #[test]
    fn create_log_detection_requires_success_and_a_create() {
        let mk = |err: serde_json::Value, log: &str| {
            serde_json::json!({"params": {"result": {"value": {
                "err": err, "signature": "SIG", "logs": [log]
            }}}})
        };
        // Happy path.
        assert_eq!(
            create_sig_from_logs(&mk(serde_json::Value::Null, "Program log: Instruction: Create")),
            Some("SIG".to_string())
        );
        // A FAILED create never minted a coin — must be ignored.
        assert_eq!(
            create_sig_from_logs(&mk(serde_json::json!({"InstructionError": []}), "Program log: Instruction: Create")),
            None
        );
        // A trade is not a launch.
        assert_eq!(
            create_sig_from_logs(&mk(serde_json::Value::Null, "Program log: Instruction: Buy")),
            None
        );
    }

    /// Live mainnet smoke test — network-dependent, so it's `#[ignore]`d by
    /// default. Run with:
    ///   cargo test --features solana live_launch_feed -- --ignored --nocapture
    ///
    /// Watches the real WebSocket feed for up to 90s. Synthetic tests only prove
    /// self-consistency; this is what catches a wrong log string or account index.
    #[tokio::test]
    #[ignore]
    async fn live_launch_feed_sees_real_launches() {
        let http = "https://api.mainnet-beta.solana.com";
        let rpc = Rpc::new(http);
        let ws = ws_url_from_http(http);
        println!("subscribing to {ws} …");

        let mut seen: Vec<Launch> = Vec::new();
        let watch = watch_launches(
            &rpc,
            &ws,
            |l| {
                println!("  LAUNCH {:<12} {}", l.symbol, l.mint);
                seen.push(l);
                seen.len() < 3 // stop after 3
            },
            |s| println!("  feed: {s}"),
        );
        let _ = tokio::time::timeout(std::time::Duration::from_secs(90), watch).await;

        assert!(!seen.is_empty(), "no launches seen in 90s — feed or decoder is wrong");
        for l in &seen {
            assert_ne!(l.mint, Pubkey::default());
            assert!(!l.symbol.is_empty());
        }
        let rows = enrich(&rpc, seen).await;
        for r in &rows {
            println!(
                "  {:<12} mcap={:>8.2} SOL pooled={:>7.4} bonded={:>5.1}%",
                r.launch.symbol, r.mkt_cap_sol(), r.pooled_sol(), r.progress() * 100.0
            );
            assert!(r.mkt_cap_sol() > 0.0);
        }
    }

    #[test]
    fn table_view_falls_back_to_sol_without_a_usd_rate() {
        let rows: Vec<TrenchRow> = Vec::new();
        let t = table_view(&rows, 0.0, None, 40);
        assert!(t.is_empty());
        assert!(!t.empty_note.is_empty(), "empty state must explain itself");
        assert_eq!(t.cols.len(), 7, "sym, pooled, mkt cap, bonded, age, risk, mint");
    }
}


// ---- live trade tape ------------------------------------------------------
//
// Trades are read from the program's own `TradeEvent`, NOT from the instruction
// arguments. The args are a cap (`max_sol_cost`) and a floor (`min_sol_output`),
// so reading them showed every buy as its limit and every sell as 0.00 — other
// bots routinely pass 0 as their floor. The event carries what actually moved,
// plus the reserves at that moment, which is also where the tape's pooled and
// market-cap figures come from.

/// Anchor `emit_cpi!` wrapper instruction on the pump program.
const DISC_CPI_EVENT: [u8; 8] = [228, 69, 165, 46, 81, 203, 154, 29];
/// The `TradeEvent` payload discriminator, from the IDL.
const DISC_TRADE_EVENT: [u8; 8] = [189, 219, 127, 211, 78, 230, 97, 238];

/// What a tape row represents.
///
/// A bool couldn't express this: liquidity events are neither buys nor sells,
/// but they matter most at launch — a deployer seeding or pulling the pool is
/// the single biggest signal about a new coin, and it moves the reserves every
/// other row is priced against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(serde::Serialize, serde::Deserialize)]
pub enum SwapKind {
    Buy,
    Sell,
    /// Liquidity added to the pool.
    AddLp,
    /// Liquidity removed — the rug shape.
    RemoveLp,
}

impl SwapKind {
    pub fn label(&self) -> &'static str {
        match self {
            SwapKind::Buy => "BUY",
            SwapKind::Sell => "SELL",
            SwapKind::AddLp => "ADD",
            SwapKind::RemoveLp => "REMOVE",
        }
    }

    /// Colour: buys/adds read as inflow, sells/removals as outflow.
    pub fn tone(&self) -> Tone {
        match self {
            SwapKind::Buy => Tone::Good,
            SwapKind::Sell => Tone::Bad,
            SwapKind::AddLp => Tone::Info,
            // The pending amber, not the sell red: pulling liquidity is the
            // row to look at, and amber reads as "attention" without claiming
            // the trade itself was a sell.
            SwapKind::RemoveLp => Tone::Warn,
        }
    }

    pub fn is_buy(&self) -> bool {
        matches!(self, SwapKind::Buy)
    }


    /// The same event read from the other side of the pair.
    ///
    /// On an inverted pool (SOL as base) the program's `BuyEvent` means someone
    /// paid the coin to receive SOL — a SELL from the coin's point of view.
    /// Liquidity events add or remove the same liquidity either way round, so
    /// they are unchanged.
    pub fn mirrored(&self) -> SwapKind {
        match self {
            SwapKind::Buy => SwapKind::Sell,
            SwapKind::Sell => SwapKind::Buy,
            other => *other,
        }
    }

    pub fn is_lp(&self) -> bool {
        matches!(self, SwapKind::AddLp | SwapKind::RemoveLp)
    }
}

/// One row on the tape: a swap or a liquidity event on this coin's pool.
/// Serde because the tape is persisted per coin (tape-<mint>.json), so the
/// trades you watched — and made — are still there after a restart.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SolSwap {
    pub kind: SwapKind,
    /// SOL that actually moved.
    pub sol: f64,
    /// Tokens that actually moved.
    pub tokens: f64,
    /// Real SOL pooled in the curve right after this trade.
    pub pooled_sol: f64,
    /// Market cap (SOL) right after this trade.
    pub mkt_cap_sol: f64,
    /// The trader.
    #[serde(with = "super::pubkey_b58")]
    pub user: Pubkey,
    pub signature: String,
    /// Which event this was WITHIN its transaction.
    ///
    /// A signature alone does not identify a trade: arbitrage and sandwich
    /// transactions execute a buy AND a sell against the same pool in one
    /// transaction. Keying on the signature alone silently discarded the second
    /// leg, so the tape showed a wall of buys while explorers showed the pairs.
    pub event_idx: usize,
    pub block_time: Option<i64>,
    /// The slot the transaction landed in — the chain's own total order.
    /// `block_time` has one-second resolution and a busy coin trades dozens
    /// of times a second, so sorting by time alone shuffled same-second rows
    /// (ties broke by SIGNATURE — alphabetical, i.e. random): the pooled
    /// column jumped around while gmgn, sorting by slot, read smoothly.
    pub slot: u64,
    /// True when the trader is us.
    pub mine: bool,
}

/// Every pump coin mints a fixed 1B supply, so market cap is supply x price.
const PUMP_TOTAL_SUPPLY: f64 = 1_000_000_000.0;

/// One entry of `TradeEvent.shareholders`.
#[derive(Debug, borsh::BorshDeserialize)]
struct Shareholder {
    _address: Pubkey,
    _share_bps: u16,
}

/// `TradeEvent`, decoded in FULL.
///
/// A prefix decode is no longer enough. pump added support for non-SOL quote
/// mints, and for those coins the legacy `sol_amount` / `*_sol_reserves` fields
/// are left at ZERO — the real numbers live in `quote_amount` and the
/// `*_quote_reserves` fields at the very end of the struct, past a
/// variable-length string and a vec. Reading only the prefix produced a tape of
/// 0.000000 SOL trades against a pool the dashboard correctly showed as live.
#[derive(Debug, borsh::BorshDeserialize)]
struct TradeEventHead {
    mint: Pubkey,
    sol_amount: u64,
    token_amount: u64,
    is_buy: bool,
    user: Pubkey,
    timestamp: i64,
    virtual_sol_reserves: u64,
    virtual_token_reserves: u64,
    real_sol_reserves: u64,
    real_token_reserves: u64,
    _fee_recipient: Pubkey,
    _fee_basis_points: u64,
    _fee: u64,
    _creator: Pubkey,
    _creator_fee_basis_points: u64,
    _creator_fee: u64,
    _track_volume: bool,
    _total_unclaimed_tokens: u64,
    _total_claimed_tokens: u64,
    _current_sol_volume: u64,
    _last_update_timestamp: i64,
    _ix_name: String,
    _mayhem_mode: bool,
    _cashback_fee_basis_points: u64,
    _cashback: u64,
    _buyback_fee_basis_points: u64,
    _buyback_fee: u64,
    _shareholders: Vec<Shareholder>,
    _quote_mint: Pubkey,
    quote_amount: u64,
    virtual_quote_reserves: u64,
    real_quote_reserves: u64,
}

impl TradeEventHead {
    /// Quote moved by this trade, in base units. Prefers the modern field and
    /// falls back to the legacy one, so both old and new events decode.
    fn quote_in(&self) -> u64 {
        if self.quote_amount > 0 { self.quote_amount } else { self.sol_amount }
    }

    /// Virtual quote reserves after the trade — the price numerator.
    fn virtual_quote(&self) -> u64 {
        if self.virtual_quote_reserves > 0 { self.virtual_quote_reserves } else { self.virtual_sol_reserves }
    }

    /// Real quote reserves after the trade — the exit liquidity.
    fn real_quote(&self) -> u64 {
        if self.real_quote_reserves > 0 { self.real_quote_reserves } else { self.real_sol_reserves }
    }
}

/// Decode a pump CPI-event instruction into a `TradeEvent`, if that's what it is.
fn decode_trade_event(data: &[u8]) -> Option<TradeEventHead> {
    // Layout: [cpi_event ix disc][event disc][borsh event]
    if data.len() < 16 || data[..8] != DISC_CPI_EVENT || data[8..16] != DISC_TRADE_EVENT {
        return None;
    }
    let mut rest = &data[16..];
    borsh::BorshDeserialize::deserialize(&mut rest).ok()
}

/// How many transactions one tape read may fetch.
///
/// The signature window and the fetch budget are deliberately DIFFERENT knobs.
/// Asking for 250 signatures is one cheap call, and a wide window is what stops
/// trades being missed. Fetching 250 transactions is 250 calls — at 8 in flight
/// on a 1.5s loop that is ~100 req/s, which rate-limited both providers into a
/// 429 storm (430 rejections in 45s). The whole round now travels as ONE
/// JSON-RPC batch request, so this cap prices a single HTTP call — 50 fits
/// every provider's batch limit and outruns the hottest launch (a 20-cap of
/// singles could not: bursts outran the fetcher and whole seconds of trades
/// scrolled past the window unfetched, which read as "we miss most
/// transactions"). Unfetched stragglers still carry to the next round.
const MAX_TX_PER_READ: usize = 50;

/// One tape read: rows decoded, plus every signature inspected.
///
/// `scanned` must include signatures that decoded to NOTHING (routing hops,
/// unrelated instructions), otherwise the caller re-fetches them forever. It
/// lists only what was actually FETCHED — deferred signatures must stay unseen.
pub struct TapeBatch {
    pub rows: Vec<SolSwap>,
    pub scanned: Vec<String>,
    /// New signatures the window held, before the fetch budget was applied.
    /// Saturation is judged on this, not on what was fetched — otherwise a
    /// budget-capped read looks permanently saturated and the window ratchets
    /// to its ceiling and stays there.
    pub fresh_total: usize,
}

/// Recent trades on a coin's bonding curve, newest first.
///
/// Reads signatures for the curve account (not the whole program), so it only
/// sees this coin. Events live in INNER instructions — trades routed through
/// aggregators have no top-level pump instruction at all, which is why a
/// top-level-only scan showed so few.
pub async fn pool_tape(
    rpc: &Rpc,
    mint: &Pubkey,
    limit: u32,
    trader: &Pubkey,
    skip: &std::collections::HashSet<String>,
) -> TapeBatch {
    let curve = bonding_curve_pda(mint);
    let sigs = match rpc.signatures_for(&curve, limit).await {
        Ok(s) => s,
        Err(_) => return TapeBatch { rows: Vec::new(), scanned: Vec::new(), fresh_total: 0 },
    };
    // A confirmed transaction never changes, so fetching one twice buys nothing.
    // This is the single biggest RPC cost in the app — the window barely moves
    // between rounds, so almost every fetch was a re-fetch.
    let mut sigs: Vec<String> = sigs.into_iter().filter(|s| !skip.contains(s)).collect();
    let fresh_total = sigs.len();
    if sigs.is_empty() {
        return TapeBatch { rows: Vec::new(), scanned: Vec::new(), fresh_total: 0 };
    }
    // Newest first from the RPC, so truncating keeps the most recent trades and
    // defers the older backlog to later rounds.
    sigs.truncate(MAX_TX_PER_READ);
    let scanned = sigs.clone();
    let fetched: Vec<(String, serde_json::Value)> = rpc
        .transactions(&sigs)
        .await
        .into_iter()
        .zip(sigs)
        .filter_map(|(tx, sig)| tx.map(|t| (sig, t)))
        .collect();

    let mut out = Vec::new();
    for (sig, tx) in fetched {
        out.extend(curve_swaps_in_tx(&tx, &sig, mint, trader));
    }
    TapeBatch { rows: out, scanned, fresh_total }
}

/// Every curve TradeEvent for `mint` inside ONE transaction. Pure, so both
/// the rolling tape and the launch-tx seed share the exact same decode.
pub fn curve_swaps_in_tx(
    tx: &serde_json::Value,
    sig: &str,
    mint: &Pubkey,
    trader: &Pubkey,
) -> Vec<SolSwap> {
    let pump_str = PUMP_PROGRAM.to_string();
    let block_time = tx.get("blockTime").and_then(|b| b.as_i64());
    let slot = tx.get("slot").and_then(|s| s.as_u64()).unwrap_or(0);
    let keys = all_account_keys(tx);
    let mut event_idx = 0usize;
    let mut out = Vec::new();

    // Events are emitted via CPI, so they are inner instructions.
    let inners = tx
        .get("meta")
        .and_then(|m| m.get("innerInstructions"))
        .and_then(|i| i.as_array())
        .cloned()
        .unwrap_or_default();
    for group in inners {
        for ix in group.get("instructions").and_then(|i| i.as_array()).into_iter().flatten() {
            let pi = ix.get("programIdIndex").and_then(|p| p.as_u64()).unwrap_or(u64::MAX) as usize;
            if keys.get(pi).map(String::as_str) != Some(pump_str.as_str()) {
                continue;
            }
            let Some(data) = ix.get("data").and_then(|d| d.as_str()).and_then(b58_decode) else { continue };
            let Some(ev) = decode_trade_event(&data) else { continue };
            if ev.mint != *mint {
                continue;
            }
            let vt = super::units_to_tokens(ev.virtual_token_reserves);
            let vq = super::lamports_to_sol(ev.virtual_quote());
            let price = if vt > 0.0 { vq / vt } else { 0.0 };
            out.push(SolSwap {
                kind: if ev.is_buy { SwapKind::Buy } else { SwapKind::Sell },
                sol: super::lamports_to_sol(ev.quote_in()),
                tokens: super::units_to_tokens(ev.token_amount),
                pooled_sol: super::lamports_to_sol(ev.real_quote()),
                // Total supply is fixed at 1B for pump coins; cap = supply x price.
                mkt_cap_sol: PUMP_TOTAL_SUPPLY * price,
                mine: ev.user == *trader,
                user: ev.user,
                signature: sig.to_string(),
                event_idx,
                block_time,
                slot,
            });
            event_idx += 1;
        }
    }
    out
}

/// The tape rows hiding in the LAUNCH transaction itself — the creator's dev
/// buy above all. On a hot snipe the create scrolls past the signature window
/// before the coin is even selected, so the tape showed everyone's trades
/// except the first and most telling one.
pub async fn tape_seed(rpc: &Rpc, launch_sig: &str, mint: &Pubkey, trader: &Pubkey) -> Vec<SolSwap> {
    match rpc.transaction(launch_sig).await {
        Ok(tx) => curve_swaps_in_tx(&tx, launch_sig, mint, trader),
        Err(_) => Vec::new(),
    }
}

// ---- AMM tape ------------------------------------------------------------

/// Anchor event discriminators for PumpSwap swaps (from `idl/pump_amm.json`).
const DISC_AMM_BUY: [u8; 8] = [103, 244, 82, 31, 44, 245, 119, 119];
const DISC_AMM_SELL: [u8; 8] = [62, 47, 55, 10, 165, 3, 220, 42];

/// The prefix `BuyEvent` and `SellEvent` share.
///
/// Both events are byte-identical through `user`: the buy's
/// `base_amount_out`/`quote_amount_in` sit at exactly the offsets the sell uses
/// for `base_amount_in`/`quote_amount_out`, so one decoder serves both and the
/// side comes from the discriminator. Fields past `user` diverge and aren't
/// needed — borsh reads a prefix, so the tail is ignored.
#[derive(Debug, borsh::BorshDeserialize)]
struct AmmTradeEvent {
    timestamp: i64,
    /// base_amount_out (buy) / base_amount_in (sell).
    base_amount: u64,
    /// max_quote_amount_in (buy) / min_quote_amount_out (sell) — the limit, not
    /// the fill. Never display this; it's the mistake the curve tape already made.
    _quote_limit: u64,
    _user_base_reserves: u64,
    _user_quote_reserves: u64,
    pool_base_reserves: u64,
    pool_quote_reserves: u64,
    /// quote_amount_in (buy) / quote_amount_out (sell), before fees.
    _quote_amount: u64,
    _lp_fee_bps: u64,
    _lp_fee: u64,
    _protocol_fee_bps: u64,
    _protocol_fee: u64,
    _quote_with_lp_fee: u64,
    /// What the user actually paid (buy) or received (sell), fees included.
    user_quote_amount: u64,
    pool: Pubkey,
    user: Pubkey,
}

/// Decode a PumpSwap swap event, returning the side alongside it.
fn decode_amm_trade_event(data: &[u8]) -> Option<(bool, AmmTradeEvent)> {
    // CPI-logged events are wrapped: [cpi_event disc][inner event disc][payload].
    let body = data.strip_prefix(&DISC_CPI_EVENT[..])?;
    let is_buy = if body.starts_with(&DISC_AMM_BUY) {
        true
    } else if body.starts_with(&DISC_AMM_SELL) {
        false
    } else {
        return None;
    };
    let mut rest = &body[8..];
    let ev: AmmTradeEvent = borsh::BorshDeserialize::deserialize(&mut rest).ok()?;
    Some((is_buy, ev))
}

/// Anchor event discriminators for PumpSwap liquidity events.
const DISC_AMM_DEPOSIT: [u8; 8] = [120, 248, 61, 83, 31, 142, 107, 144];
const DISC_AMM_WITHDRAW: [u8; 8] = [22, 9, 133, 26, 160, 44, 71, 192];

/// The prefix `DepositEvent` and `WithdrawEvent` share.
///
/// Identical through `user`, exactly like the buy/sell pair: the deposit's
/// `base_amount_in`/`quote_amount_in` occupy the offsets the withdraw uses for
/// its `_out` amounts, so the side comes from the discriminator.
#[derive(Debug, borsh::BorshDeserialize)]
struct AmmLpEvent {
    timestamp: i64,
    /// LP tokens minted (deposit) or burned (withdraw).
    _lp_token_amount: u64,
    /// The caller's slippage limits, NOT the fill.
    _limit_base: u64,
    _limit_quote: u64,
    _user_base_reserves: u64,
    _user_quote_reserves: u64,
    pool_base_reserves: u64,
    pool_quote_reserves: u64,
    /// Base tokens actually moved.
    base_amount: u64,
    /// SOL actually moved — the number that matters for a rug.
    quote_amount: u64,
    _lp_mint_supply: u64,
    pool: Pubkey,
    user: Pubkey,
}

/// `CreatePoolEvent` — the pool's initial seeding at graduation.
const DISC_AMM_CREATE_POOL: [u8; 8] = [177, 49, 12, 210, 160, 118, 167, 116];

/// The prefix of `CreatePoolEvent` we need.
///
/// This is the launch's FIRST liquidity add — a deployer seeding the pool — and
/// it is a different event from `DepositEvent`, so a deposit-only decoder misses
/// the single most important LP moment a graduated coin has.
#[derive(Debug, borsh::BorshDeserialize)]
struct AmmCreatePoolEvent {
    timestamp: i64,
    _index: u16,
    creator: Pubkey,
    _base_mint: Pubkey,
    _quote_mint: Pubkey,
    _base_decimals: u8,
    _quote_decimals: u8,
    base_amount_in: u64,
    quote_amount_in: u64,
    pool_base_amount: u64,
    pool_quote_amount: u64,
    _minimum_liquidity: u64,
    _initial_liquidity: u64,
    _lp_token_amount_out: u64,
    _pool_bump: u8,
    pool: Pubkey,
}

fn decode_amm_create_pool(data: &[u8]) -> Option<AmmCreatePoolEvent> {
    let body = data.strip_prefix(&DISC_CPI_EVENT[..])?;
    if !body.starts_with(&DISC_AMM_CREATE_POOL) {
        return None;
    }
    let mut rest = &body[8..];
    borsh::BorshDeserialize::deserialize(&mut rest).ok()
}

/// Decode a PumpSwap liquidity event, returning whether it ADDED liquidity.
fn decode_amm_lp_event(data: &[u8]) -> Option<(SwapKind, AmmLpEvent)> {
    let body = data.strip_prefix(&DISC_CPI_EVENT[..])?;
    let kind = if body.starts_with(&DISC_AMM_DEPOSIT) {
        SwapKind::AddLp
    } else if body.starts_with(&DISC_AMM_WITHDRAW) {
        SwapKind::RemoveLp
    } else {
        return None;
    };
    let mut rest = &body[8..];
    let ev: AmmLpEvent = borsh::BorshDeserialize::deserialize(&mut rest).ok()?;
    Some((kind, ev))
}

/// Recent trades for a GRADUATED coin, read from its PumpSwap pool.
///
/// A graduated coin's bonding curve is dead — every trade happens against the
/// AMM pool — so reading the curve returned the same handful of launch-day
/// trades forever while the coin traded live elsewhere. Signatures come from the
/// pool account, which every swap must reference.
pub async fn amm_tape(
    rpc: &Rpc,
    pool: &Pubkey,
    limit: u32,
    trader: &Pubkey,
    token_decimals: u8,
    sol_is_base: bool,
    // Real circulating supply, for market cap. Listed coins are not all 1B:
    // assuming so made the tape's cap read 100x low against the market panel.
    total_supply: f64,
    skip: &std::collections::HashSet<String>,
) -> TapeBatch {
    let sigs = match rpc.signatures_for(pool, limit).await {
        Ok(s) => s,
        Err(_) => return TapeBatch { rows: Vec::new(), scanned: Vec::new(), fresh_total: 0 },
    };
    let mut sigs: Vec<String> = sigs.into_iter().filter(|s| !skip.contains(s)).collect();
    let fresh_total = sigs.len();
    if sigs.is_empty() {
        return TapeBatch { rows: Vec::new(), scanned: Vec::new(), fresh_total: 0 };
    }
    // Newest first from the RPC, so truncating keeps the most recent trades and
    // defers the older backlog to later rounds.
    sigs.truncate(MAX_TX_PER_READ);
    let scanned = sigs.clone();
    let amm_str = super::PUMP_AMM_PROGRAM.to_string();
    let fetched: Vec<(String, serde_json::Value)> = rpc
        .transactions(&sigs)
        .await
        .into_iter()
        .zip(sigs)
        .filter_map(|(tx, sig)| tx.map(|t| (sig, t)))
        .collect();

    let scale = 10f64.powi(token_decimals as i32);
    let mut out = Vec::new();
    for (sig, tx) in fetched {
        let block_time = tx.get("blockTime").and_then(|b| b.as_i64());
        let slot = tx.get("slot").and_then(|s| s.as_u64()).unwrap_or(0);
        let keys = all_account_keys(&tx);
        let mut event_idx = 0usize;
        let inners = tx
            .get("meta")
            .and_then(|m| m.get("innerInstructions"))
            .and_then(|i| i.as_array())
            .cloned()
            .unwrap_or_default();
        for group in inners {
            for ix in group.get("instructions").and_then(|i| i.as_array()).into_iter().flatten() {
                let pi = ix.get("programIdIndex").and_then(|p| p.as_u64()).unwrap_or(u64::MAX) as usize;
                if keys.get(pi).map(String::as_str) != Some(amm_str.as_str()) {
                    continue;
                }
                let Some(data) = ix.get("data").and_then(|d| d.as_str()).and_then(b58_decode) else { continue };

                // A swap, or a liquidity event — both matter on the tape.
                let row = if let Some((is_buy, ev)) = decode_amm_trade_event(&data) {
                    Some((
                        if is_buy { SwapKind::Buy } else { SwapKind::Sell },
                        ev.pool,
                        ev.user,
                        ev.timestamp,
                        ev.user_quote_amount,
                        ev.base_amount,
                        ev.pool_base_reserves,
                        ev.pool_quote_reserves,
                    ))
                } else if let Some((kind, ev)) = decode_amm_lp_event(&data) {
                    Some((
                        kind,
                        ev.pool,
                        ev.user,
                        ev.timestamp,
                        ev.quote_amount,
                        ev.base_amount,
                        ev.pool_base_reserves,
                        ev.pool_quote_reserves,
                    ))
                } else {
                    // The launch's own liquidity seeding.
                    decode_amm_create_pool(&data).map(|ev| {
                        (
                            SwapKind::AddLp,
                            ev.pool,
                            ev.creator,
                            ev.timestamp,
                            ev.quote_amount_in,
                            ev.base_amount_in,
                            ev.pool_base_amount,
                            ev.pool_quote_amount,
                        )
                    })
                };
                let Some((kind, ev_pool, user, ts, quote_raw, base_raw, base_res, quote_res)) = row
                else {
                    continue;
                };
                // A pool account can be referenced by unrelated routing hops.
                if ev_pool != *pool {
                    continue;
                }
                // Events speak the pool's BASE/QUOTE slots. On an inverted pool
                // SOL is the base, so every amount swaps sides — and the sense
                // of the event flips too: paying quote (the coin) to receive
                // base (SOL) is a SELL, even though it is a `BuyEvent`.
                let (kind, sol_raw, tok_raw, sol_res, tok_res) = if sol_is_base {
                    (kind.mirrored(), base_raw, quote_raw, base_res, quote_res)
                } else {
                    (kind, quote_raw, base_raw, quote_res, base_res)
                };
                let base = tok_res as f64 / scale;
                let quote = super::lamports_to_sol(sol_res);
                let price = if base > 0.0 { quote / base } else { 0.0 };
                out.push(SolSwap {
                    kind,
                    sol: super::lamports_to_sol(sol_raw),
                    tokens: tok_raw as f64 / scale,
                    // On the AMM the pool's quote balance IS the exit liquidity.
                    pooled_sol: quote,
                    mkt_cap_sol: total_supply * price,
                    mine: user == *trader,
                    user,
                    signature: sig.clone(),
                    event_idx,
                    block_time: block_time.or(Some(ts)),
                    slot,
                });
                event_idx += 1;
            }
        }
    }
    TapeBatch { rows: out, scanned, fresh_total }
}

/// First 6 and last 4 of a pubkey — enough to match against an explorer's
/// maker column without eating the row.
fn short_key(k: &Pubkey) -> String {
    let s = k.to_string();
    if s.len() <= 12 {
        return s;
    }
    format!("{}…{}", &s[..6], &s[s.len() - 4..])
}

/// Render the tape into the shared table model — same columns as the EVM tape.
pub fn tape_view(swaps: &[SolSwap], scroll: usize, h: usize, sol_usd: f64) -> TableView {
    let total = swaps.len();
    // Window like the Orders panel — the tape now accumulates hundreds of rows,
    // so rendering all of them meant everything past the first screenful was
    // unreachable.
    let start = scroll.min(total.saturating_sub(1).max(0));
    let end = (start + h).min(total);
    let swaps = &swaps[start.min(total)..end];
    let mut t = TableView::new(
        if total > h {
            format!(" Trades {}–{} of {} ↑/↓ scroll ⭐ = you [l] ", start + 1, end, total)
        } else {
            format!(" Trades ({total}) ⭐ = you [l] ")
        },
        vec![
            Col::fixed("", 2),
            // Age leads: a tape is read newest-first, so "how long ago" is the
            // first thing you want, not the last.
            Col::fixed("age", 6),
            Col::fixed("action", 8),
            Col::fixed("amount SOL", 13),
            Col::fixed("tokens", 14),
            Col::fixed("pooled SOL", 12),
            Col::fixed("mkt cap $", 12),
            // Maker and signature in full: both exist to be matched against an
            // explorer or pasted into one, and a shortened key does neither.
            Col::fixed("trader", 44),
            Col::min("sig", 88),
        ],
    );
    t.empty_note = "no trades seen yet".into();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    for s in swaps {
        let age = s
            .block_time
            .map(|bt| view::age_compact((now - bt).max(0) as f64))
            .unwrap_or_else(|| "—".into());
        t.push_mine(
            // Colour only what the SIDE is about: the action, the liquidity it
            // moved, and the cap that implies. Amount, tokens, trader and the
            // signature are facts that read the same either way, and colouring
            // them turned every row into a block of green or red.
            vec![
                Cell::toned(if s.mine { view::MINE_MARK } else { "" }, Tone::Warn),
                Cell::new(age),
                Cell::bold(s.kind.label(), s.kind.tone()),
                Cell::new(format!("{:.6}", s.sol)),
                Cell::new(format!("{:.0}", s.tokens)),
                Cell::toned(view::sol_compact(s.pooled_sol), s.kind.tone()),
                // Market cap in dollars: SOL is the right unit for pooled
                // liquidity (that IS the exit), but a cap denominated in SOL
                // means nothing at a glance.
                Cell::toned(
                    if sol_usd > 0.0 {
                        view::usd_compact(s.mkt_cap_sol * sol_usd)
                    } else {
                        format!("{:.0} SOL", s.mkt_cap_sol)
                    },
                    s.kind.tone(),
                ),
                Cell::toned(s.user.to_string(), if s.mine { Tone::Warn } else { Tone::Normal }),
                // Full signature — truncating makes it useless for lookup.
                Cell::toned(s.signature.clone(), Tone::Normal),
            ],
            s.mine,
        );
    }
    t
}

#[cfg(test)]
mod shape_tests {
    use super::*;
    use solana_pubkey::Pubkey;

    fn row(sym: &str, block_time: Option<i64>) -> TrenchRow {
        TrenchRow {
            launch: Launch {
                mint: Pubkey::new_from_array([7u8; 32]),
                name: sym.into(),
                symbol: sym.into(),
                signature: "sig".into(),
                block_time,
            },
            curve: BondingCurve {
                virtual_token_reserves: 1_073_000_000_000_000,
                virtual_quote_reserves: 30_000_000_000,
                real_token_reserves: 793_100_000_000_000,
                real_quote_reserves: 1_000_000_000,
                token_total_supply: 1_000_000_000_000_000,
                complete: false,
                creator: Pubkey::default(),
                is_mayhem_mode: false,
                is_cashback_coin: false,
                quote_mint: Pubkey::default(),
            },
        }
    }

    /// Regression: the `age` column was added without its cell, so every row was
    /// one short and the mint rendered under "age".
    #[test]
    fn trenches_rows_match_their_columns() {
        let rows = vec![row("AAA", Some(0)), row("BBB", None)];
        let t = table_view(&rows, 0.0, None, 40);
        assert!(t.is_well_formed(), "row/column count mismatch shifts every column");
        assert_eq!(t.cols.len(), 7);
    }

    /// A missing block time must render a placeholder, not an empty cell that
    /// looks like a layout bug.
    #[test]
    fn age_falls_back_when_block_time_is_absent() {
        assert_eq!(row("X", None).age_text(), "—");
        assert_ne!(row("X", Some(0)).age_text(), "—");
    }

    #[test]
    fn tape_rows_match_their_columns() {
        let swaps = vec![SolSwap {
            kind: SwapKind::Buy,
            sol: 0.5,
            tokens: 1_000.0,
            pooled_sol: 2.5,
            mkt_cap_sol: 30.0,
            user: Pubkey::default(),
            signature: "abcdefghijklmno".into(),
            event_idx: 0,
            block_time: Some(0),
            slot: 1,
            mine: true,
        }];
        assert!(tape_view(&swaps, 0, 20, 150.0).is_well_formed());
    }
}


/// Live SOL/USD from pump.fun's own price endpoint.
///
/// Used to show market caps in dollars alongside SOL — 28 SOL means little at a
/// glance, "$2.1k" is immediately legible. Returns `None` on failure rather than
/// guessing a rate; the UI then stays in SOL rather than showing a made-up
/// dollar figure.
pub async fn sol_usd(client: &reqwest::Client) -> Option<f64> {
    let req = client
        .get("https://frontend-api-v3.pump.fun/sol-price")
        .header("User-Agent", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36")
        .send();
    let resp = tokio::time::timeout(std::time::Duration::from_secs(6), req).await.ok()?.ok()?;
    let v: serde_json::Value = resp.json().await.ok()?;
    v.get("solPrice").and_then(|p| p.as_f64()).filter(|p| *p > 0.0)
}

#[cfg(test)]
mod live_tape_tests {
    use super::*;

    /// Live check that the tape actually decodes trades.
    ///
    /// The failure this guards against is silent: v0 transactions resolve program
    /// ids through Address Lookup Tables, so reading only `message.accountKeys`
    /// skips every ALT-routed instruction and the view just looks quiet.
    ///
    ///   cargo test --features solana live_tape -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn live_tape_decodes_real_trades() {
        let rpc = Rpc::new("https://api.mainnet-beta.solana.com");
        // Take a coin off the live feed so the test always has a fresh target.
        let launches = scan_launches(&rpc, 30).await;
        println!("launches found: {}", launches.len());

        // Fall back to scanning the program for any active curve.
        let mint = match launches.first() {
            Some(l) => l.mint,
            None => {
                println!("no launches in window; skipping");
                return;
            }
        };
        println!("probing mint {mint}");
        let me = Pubkey::default();
        let batch = pool_tape(&rpc, &mint, 20, &me, &std::collections::HashSet::new()).await;
        let swaps = batch.rows;
        println!("decoded {} trade(s) from {} signature(s)", swaps.len(), batch.scanned.len());
        for s in swaps.iter().take(6) {
            println!(
                "  {:<4} sol={:<12.6} tokens={:<14.0} pooled={:<10.4} mcap={:.2}",
                s.kind.label(),
                s.sol,
                s.tokens,
                s.pooled_sol,
                s.mkt_cap_sol
            );
        }
        for s in &swaps {
            assert!(s.sol > 0.0, "a decoded trade must have a real SOL amount");
            assert!(s.tokens > 0.0, "and a real token amount");
        }
    }
}

#[cfg(test)]
mod live_launch_latency {
    use super::*;

    /// Live: run the real launch feed for a while and measure how far behind
    /// the chain each discovery is (detect time vs the tx's own blockTime),
    /// then seed the tape from each launch tx and report whether the dev buy
    /// decoded. The two numbers the trenches live or die on.
    ///   cargo test --features solana live_launch_latency -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn live_launch_latency_and_dev_buy() {
        let reg = crate::config::Registry::load_or_create().expect("registry").0;
        let net = reg.networks.iter().find(|n| n.kind.is_solana()).expect("solana net");
        let rpc = crate::sol::rpc::Rpc::new_pool(net.rpc_pool());
        let ws = net.ws_pool();
        let mut urls = ws.clone();
        for u in net.rpc_pool() {
            let d = ws_url_from_http(&u);
            if !d.is_empty() && !urls.contains(&d) {
                urls.push(d);
            }
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let rpc2 = rpc.clone();
        let feed = tokio::spawn(async move {
            let _ = watch_launches_ha(
                &rpc2,
                &urls,
                |l| {
                    let _ = tx.send((l, std::time::SystemTime::now()));
                    true
                },
                |m| println!("  feed: {m}"),
            )
            .await;
        });

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut n = 0u32;
        while let Ok(Some((l, seen))) = tokio::time::timeout_at(deadline, rx.recv()).await {
            n += 1;
            let seen_unix = seen.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
            let lag = l.block_time.map(|bt| seen_unix - bt as f64);
            let seed = tape_seed(&rpc, &l.signature, &l.mint, &Pubkey::default()).await;
            let dev_buy = seed.iter().any(|s| matches!(s.kind, SwapKind::Buy));
            println!(
                "  {} {:<12} lag={} dev_buy={} seed_rows={}",
                l.mint,
                l.symbol.chars().take(12).collect::<String>(),
                lag.map(|s| format!("{s:+.2}s")).unwrap_or_else(|| "?".into()),
                dev_buy,
                seed.len(),
            );
            if n >= 12 {
                break;
            }
        }
        feed.abort();
        println!("  saw {n} launches");
        assert!(n > 0, "no launches in 60s — feed dead or market asleep");
    }
}

#[cfg(test)]
mod live_ws_probe {
    /// Which websocket endpoints actually accept an upgrade, using a real WS
    /// client. A plain-HTTP probe is not a substitute: some providers answer
    /// 404 to `curl -H "Upgrade: websocket"` yet accept a genuine wss:// dial.
    ///   cargo test --features solana live_ws_endpoint_probe -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn live_ws_endpoint_probe() {
        let reg = crate::config::Registry::load("deployments.json").expect("registry");
        let net = reg
            .networks
            .iter()
            .find(|n| n.kind.is_solana())
            .expect("a solana network");

        let mut cands: Vec<String> = net.ws_pool();
        for u in net.rpc_pool() {
            cands.push(super::ws_url_from_http(&u));
        }

        for url in cands {
            let normalised = super::normalize_ws_url(&url);
            let safe = normalised.split('?').next().unwrap_or("").to_string();
            match tokio::time::timeout(
                std::time::Duration::from_secs(10),
                tokio_tungstenite::connect_async(normalised.as_str()),
            )
            .await
            {
                Ok(Ok(_)) => println!("  OK    {safe}"),
                Ok(Err(e)) => println!("  FAIL  {safe}  -> {e}"),
                Err(_) => println!("  TIMEOUT {safe}"),
            }
        }
    }
}

#[cfg(test)]
mod trench_sort_tests {
    use super::*;

    fn row(mint_byte: u8, t: Option<i64>) -> TrenchRow {
        TrenchRow {
            launch: Launch {
                mint: Pubkey::new_from_array([mint_byte; 32]),
                name: "n".into(),
                symbol: "s".into(),
                signature: "sig".into(),
                block_time: t,
            },
            curve: BondingCurve {
                virtual_token_reserves: 1,
                virtual_quote_reserves: 1,
                real_token_reserves: 0,
                real_quote_reserves: 0,
                token_total_supply: 1,
                complete: false,
                creator: Pubkey::new_unique(),
                is_mayhem_mode: false,
                is_cashback_coin: false,
                quote_mint: Pubkey::default(),
            },
        }
    }

    #[test]
    fn newest_launch_sorts_to_the_top() {
        let mut rows = vec![row(1, Some(100)), row(2, Some(300)), row(3, Some(200))];
        sort_newest_first(&mut rows);
        let times: Vec<Option<i64>> = rows.iter().map(|r| r.launch.block_time).collect();
        assert_eq!(times, vec![Some(300), Some(200), Some(100)]);
    }

    #[test]
    fn identical_timestamps_hold_a_stable_order() {
        // Launches land in bursts within the same second; without a tiebreak the
        // list reshuffled on every redraw.
        let mut a = vec![row(3, Some(9)), row(1, Some(9)), row(2, Some(9))];
        let mut b = vec![row(2, Some(9)), row(3, Some(9)), row(1, Some(9))];
        sort_newest_first(&mut a);
        sort_newest_first(&mut b);
        let ka: Vec<Pubkey> = a.iter().map(|r| r.launch.mint).collect();
        let kb: Vec<Pubkey> = b.iter().map(|r| r.launch.mint).collect();
        assert_eq!(ka, kb);
    }

    #[test]
    fn launches_without_a_block_time_sort_last() {
        let mut rows = vec![row(1, None), row(2, Some(5))];
        sort_newest_first(&mut rows);
        assert_eq!(rows[0].launch.block_time, Some(5));
    }
}

#[cfg(test)]
mod lp_event_tests {
    use super::*;

    #[test]
    fn lp_events_are_distinct_from_swaps() {
        // The whole point of the enum: a liquidity pull must never render as a
        // sell, and must never be counted as one.
        assert!(!SwapKind::RemoveLp.is_buy());
        assert!(!SwapKind::AddLp.is_buy());
        assert!(SwapKind::AddLp.is_lp() && SwapKind::RemoveLp.is_lp());
        assert!(!SwapKind::Buy.is_lp() && !SwapKind::Sell.is_lp());
        assert_eq!(SwapKind::AddLp.label(), "ADD");
        assert_eq!(SwapKind::RemoveLp.label(), "REMOVE");
        // Both liquidity kinds must be distinguishable from a plain buy/sell:
        // ADD gets its own colour, REMOVE the attention amber.
        assert_ne!(SwapKind::AddLp.tone(), SwapKind::Buy.tone());
        assert_eq!(SwapKind::RemoveLp.tone(), Tone::Warn);
        assert_ne!(SwapKind::RemoveLp.tone(), SwapKind::Sell.tone());
    }

    #[test]
    fn pool_creation_counts_as_adding_liquidity() {
        let mut ev = Vec::new();
        ev.extend_from_slice(&DISC_CPI_EVENT);
        ev.extend_from_slice(&DISC_AMM_CREATE_POOL);
        ev.extend_from_slice(&[0u8; 300]);
        assert!(decode_amm_create_pool(&ev).is_some());
        // And must not be mistaken for a swap or a deposit.
        assert!(decode_amm_trade_event(&ev).is_none());
        assert!(decode_amm_lp_event(&ev).is_none());
    }

    #[test]
    fn lp_and_swap_discriminators_are_all_distinct() {
        let discs = [
            DISC_AMM_BUY,
            DISC_AMM_SELL,
            DISC_AMM_DEPOSIT,
            DISC_AMM_WITHDRAW,
            DISC_AMM_CREATE_POOL,
        ];
        for (i, a) in discs.iter().enumerate() {
            for b in discs.iter().skip(i + 1) {
                assert_ne!(a, b, "a shared discriminator would misclassify events");
            }
        }
    }

    #[test]
    fn a_swap_payload_is_not_decoded_as_a_liquidity_event() {
        // Both decoders read the same CPI wrapper, so each must reject the
        // other's payload rather than silently misreading its fields.
        let mut buy = Vec::new();
        buy.extend_from_slice(&DISC_CPI_EVENT);
        buy.extend_from_slice(&DISC_AMM_BUY);
        buy.extend_from_slice(&[0u8; 200]);
        assert!(decode_amm_lp_event(&buy).is_none());
        assert!(decode_amm_trade_event(&buy).is_some());

        let mut dep = Vec::new();
        dep.extend_from_slice(&DISC_CPI_EVENT);
        dep.extend_from_slice(&DISC_AMM_DEPOSIT);
        dep.extend_from_slice(&[0u8; 200]);
        assert!(decode_amm_trade_event(&dep).is_none());
        assert_eq!(decode_amm_lp_event(&dep).map(|(k, _)| k), Some(SwapKind::AddLp));
    }
}

#[cfg(test)]
mod tape_view_tests {
    use super::*;

    fn swaps(n: usize) -> Vec<SolSwap> {
        (0..n)
            .map(|i| SolSwap {
                kind: SwapKind::Buy,
                sol: 1.0,
                tokens: 1.0,
                pooled_sol: 1.0,
                mkt_cap_sol: 1.0,
                user: Pubkey::new_unique(),
                signature: format!("s{i}"),
                event_idx: 0,
                block_time: Some(i as i64),
                slot: i as u64,
                mine: false,
            })
            .collect()
    }

    #[test]
    fn tape_view_windows_rows() {
        let all = swaps(50);
        let t = tape_view(&all, 10, 5, 150.0);
        assert_eq!(t.rows.len(), 5, "only the visible window renders");
        assert!(t.title.contains("11–15 of 50"), "title should locate the window: {}", t.title);
        assert!(t.is_well_formed());
    }

    #[test]
    fn a_short_tape_needs_no_window() {
        let t = tape_view(&swaps(3), 0, 20, 150.0);
        assert_eq!(t.rows.len(), 3);
        assert!(t.title.contains("(3)"));
    }

    #[test]
    fn scrolling_past_the_end_does_not_panic() {
        // The tape shrinks when a coin is switched while scrolled down.
        let t = tape_view(&swaps(4), 99, 10, 150.0);
        assert!(t.rows.len() <= 4);
        let empty = tape_view(&[], 5, 10, 150.0);
        assert!(empty.rows.is_empty());
    }
}

#[cfg(test)]
mod tape_merge_tests {
    use super::*;

    fn swap(sig: &str, t: i64) -> SolSwap {
        SolSwap {
            kind: SwapKind::Buy,
            sol: 1.0,
            tokens: 1.0,
            pooled_sol: 1.0,
            mkt_cap_sol: 1.0,
            user: Pubkey::new_unique(),
            signature: sig.to_string(),
            event_idx: 0,
            block_time: Some(t),
            slot: t.max(0) as u64,
            mine: false,
        }
    }

    #[test]
    fn history_accumulates_instead_of_being_replaced() {
        let mut tape = Vec::new();
        merge_tape(&mut tape, vec![swap("c", 3), swap("b", 2)]);
        // A later poll window that no longer contains "b" must not drop it:
        // that wholesale replace is what made trades vanish after seconds.
        merge_tape(&mut tape, vec![swap("d", 4), swap("c", 3)]);
        let sigs: Vec<&str> = tape.iter().map(|t| t.signature.as_str()).collect();
        assert_eq!(sigs, vec!["d", "c", "b"], "newest first, nothing lost");
    }

    fn leg(sig: &str, idx: usize, kind: SwapKind) -> SolSwap {
        let mut s = swap(sig, 1);
        s.event_idx = idx;
        s.kind = kind;
        s
    }

    #[test]
    fn arbitrage_legs_in_one_transaction_both_survive() {
        // A sandwich/arb transaction buys AND sells the same pool. Keying on
        // the signature alone dropped the sell, so the tape showed only buys
        // while explorers showed the pairs.
        let mut tape = Vec::new();
        let added = merge_tape(
            &mut tape,
            vec![leg("arb", 0, SwapKind::Buy), leg("arb", 1, SwapKind::Sell)],
        );
        assert_eq!(added.len(), 2, "both legs are distinct trades");
        assert_eq!(tape.len(), 2);
        assert!(tape.iter().any(|t| t.kind == SwapKind::Sell), "the sell must survive");
        // Re-seeing the same transaction still adds nothing.
        assert!(merge_tape(&mut tape, vec![leg("arb", 0, SwapKind::Buy)]).is_empty());
        assert_eq!(tape.len(), 2);
    }

    #[test]
    fn a_batch_is_self_deduplicating() {
        // The same (signature, event) arriving twice in one batch is a genuine
        // duplicate and must collapse — unlike two DIFFERENT events sharing a
        // signature, which are separate trades.
        let mut tape = Vec::new();
        merge_tape(&mut tape, vec![swap("dup", 1), swap("dup", 1), swap("other", 2)]);
        assert_eq!(tape.len(), 2);
    }

    #[test]
    fn merge_reports_only_genuinely_new_rows() {
        let mut tape = Vec::new();
        assert_eq!(merge_tape(&mut tape, vec![swap("a", 1)]).len(), 1);
        // Re-seeing it must report nothing, or the log narrates it twice.
        assert_eq!(merge_tape(&mut tape, vec![swap("a", 1)]).len(), 0);
    }

    #[test]
    fn duplicate_signatures_are_never_shown_twice() {
        let mut tape = Vec::new();
        merge_tape(&mut tape, vec![swap("a", 1)]);
        merge_tape(&mut tape, vec![swap("a", 1), swap("a", 1)]);
        assert_eq!(tape.len(), 1);
    }

    #[test]
    fn same_block_trades_hold_a_stable_order() {
        // Providers disagree about ordering within a block; without a tiebreak
        // the rows swapped places every redraw.
        let mut a = Vec::new();
        merge_tape(&mut a, vec![swap("x", 9), swap("y", 9), swap("z", 9)]);
        let mut b = Vec::new();
        merge_tape(&mut b, vec![swap("z", 9), swap("x", 9), swap("y", 9)]);
        let sa: Vec<&str> = a.iter().map(|t| t.signature.as_str()).collect();
        let sb: Vec<&str> = b.iter().map(|t| t.signature.as_str()).collect();
        assert_eq!(sa, sb, "arrival order must not affect display order");
    }

    #[test]
    fn the_ring_is_bounded() {
        let mut tape = Vec::new();
        for round in 0..40 {
            let batch: Vec<SolSwap> = (0..40)
                .map(|i| swap(&format!("s{round}-{i}"), (round * 40 + i) as i64))
                .collect();
            merge_tape(&mut tape, batch);
        }
        assert_eq!(tape.len(), TAPE_RING, "a long session must not grow forever");
        // Truncation keeps the NEWEST trades.
        assert_eq!(tape[0].block_time, Some(1599));
    }

    #[test]
    fn unknown_block_times_sort_last() {
        let mut tape = Vec::new();
        let mut no_time = swap("q", 0);
        no_time.block_time = None;
        merge_tape(&mut tape, vec![no_time, swap("p", 5)]);
        assert_eq!(tape[0].signature, "p", "a timestamped trade outranks an unknown one");
    }
}

#[cfg(test)]
mod live_amm_tape_tests {
    use super::*;

    /// Decode real PumpSwap trades for a graduated coin.
    ///   MINT=<base58> cargo test --features solana live_amm_tape -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn live_amm_tape_decodes_real_trades() {
        let reg = crate::config::Registry::load("deployments.json").expect("registry");
        let net = reg.networks.iter().find(|n| n.kind.is_solana()).expect("solana net");
        let rpc = Rpc::new_pool(net.rpc_pool());

        let mint: Pubkey = std::env::var("MINT")
            .unwrap_or_else(|_| "9cRCn9rGT8V2imeM2BaKs13yhMEais3ruM3rPvTGpump".into())
            .parse()
            .expect("valid mint");

        let (pool_key, _pool, _virtual) = super::super::pumpswap::find_pool(&rpc, &mint)
            .await
            .expect("graduated coin should have a pool");
        println!("pool {pool_key}");

        let trades = amm_tape(&rpc, &pool_key, 12, &Pubkey::new_unique(), 6, false, 1e9, &std::collections::HashSet::new())
            .await
            .rows;
        println!("decoded {} trades", trades.len());
        for t in trades.iter().take(8) {
            println!(
                "  {:<4} {:>12.6} SOL  {:>14.0} tok  pooled {:>10.2}  cap {:>12.0}",
                t.kind.label(),
                t.sol,
                t.tokens,
                t.pooled_sol,
                t.mkt_cap_sol
            );
        }
        assert!(!trades.is_empty(), "an active graduated coin must show trades");
        // The bug this replaces showed launch-day curve trades: sub-1-SOL pools.
        assert!(
            trades[0].pooled_sol > 1.0,
            "pooled SOL should reflect the AMM pool, not a dead curve"
        );
    }
}

#[cfg(test)]
mod live_coverage_tests {
    use super::*;
    use std::collections::HashSet;

    /// Does windowed polling MISS trades on a busy pool?
    ///
    /// Simulates the poller (repeated narrow windows with dedup) and compares
    /// against a ground-truth wide scan of the same period. Any signature in the
    /// wide scan that polling never saw is a trade the tape would have lost.
    ///
    ///   MINT=<base58> cargo test --features solana live_polling_loses -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn live_polling_loses_no_trades() {
        let reg = crate::config::Registry::load("deployments.json").expect("registry");
        let net = reg.networks.iter().find(|n| n.kind.is_solana()).expect("solana net");
        let rpc = Rpc::new_pool(net.rpc_pool());
        let mint: Pubkey = std::env::var("MINT")
            .unwrap_or_else(|_| "9cRCn9rGT8V2imeM2BaKs13yhMEais3ruM3rPvTGpump".into())
            .parse()
            .expect("valid mint");

        let (pool, _, _) = super::super::pumpswap::find_pool(&rpc, &mint).await.expect("pool");
        let newest = |v: &Vec<String>| v.first().cloned();

        // Mark the start of the observation window.
        let start = rpc.signatures_for(&pool, 1).await.unwrap_or_default();
        let start_sig = newest(&start);
        println!("watching pool {pool} from {start_sig:?}");

        // Poll the way the app does: narrow window, dedup, every 1.5s.
        let mut seen: HashSet<String> = HashSet::new();
        for round in 0..6 {
            let sigs = rpc.signatures_for(&pool, 40).await.unwrap_or_default();
            let fresh: Vec<&String> = sigs.iter().filter(|s| !seen.contains(*s)).collect();
            println!("  round {round}: {} returned, {} new", sigs.len(), fresh.len());
            for s in sigs {
                seen.insert(s);
            }
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        }

        // Ground truth: one wide scan covering the whole period.
        let truth = rpc.signatures_for(&pool, 250).await.unwrap_or_default();
        let cutoff = match start_sig.as_ref().and_then(|s| truth.iter().position(|t| t == s)) {
            Some(i) => i,
            None => {
                println!("start signature aged out of a 250 window — pool too hot to judge");
                return;
            }
        };
        let expected: Vec<&String> = truth[..cutoff].iter().collect();
        let missed: Vec<&&String> = expected.iter().filter(|s| !seen.contains(**s)).collect();
        println!("  {} trades in window, {} missed", expected.len(), missed.len());
        for m in missed.iter().take(5) {
            println!("    MISSED {m}");
        }
        assert!(missed.is_empty(), "windowed polling dropped {} trades", missed.len());
    }
}

#[cfg(test)]
mod live_event_census {
    use super::*;

    /// Count RAW pump-AMM event discriminators in a pool's recent transactions.
    ///
    /// Answers "does the chain actually contain the sells the explorer shows",
    /// independent of how we classify or merge them.
    ///   MINT=<base58> cargo test --features solana live_raw_event_census -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn live_raw_event_census() {
        let reg = crate::config::Registry::load("deployments.json").expect("registry");
        let net = reg.networks.iter().find(|n| n.kind.is_solana()).expect("solana net");
        let rpc = Rpc::new_pool(net.rpc_pool());
        let mint: Pubkey = std::env::var("MINT")
            .unwrap_or_else(|_| "DtSA9ReBXyvJqtoJNKNnLToUGgjxjnypwY1o5y7aBX8o".into())
            .parse()
            .expect("valid mint");
        let (pool, p, _) = super::super::pumpswap::find_pool(&rpc, &mint).await.expect("pool");
        println!("pool {pool}  sol_is_base={}", p.is_sol_based());

        let sigs = rpc.signatures_for(&pool, 12).await.unwrap_or_default();
        let amm = super::super::PUMP_AMM_PROGRAM.to_string();
        let (mut buys, mut sells, mut multi) = (0, 0, 0);
        for sig in &sigs {
            let Ok(tx) = rpc.transaction(sig).await else { continue };
            let keys = all_account_keys(&tx);
            let mut per_tx = Vec::new();
            for group in tx.get("meta").and_then(|m| m.get("innerInstructions"))
                .and_then(|i| i.as_array()).cloned().unwrap_or_default() {
                for ix in group.get("instructions").and_then(|i| i.as_array()).into_iter().flatten() {
                    let pi = ix.get("programIdIndex").and_then(|p| p.as_u64()).unwrap_or(u64::MAX) as usize;
                    if keys.get(pi).map(String::as_str) != Some(amm.as_str()) { continue; }
                    let Some(d) = ix.get("data").and_then(|x| x.as_str()).and_then(b58_decode) else { continue };
                    let Some(body) = d.strip_prefix(&DISC_CPI_EVENT[..]) else { continue };
                    if body.starts_with(&DISC_AMM_BUY) { per_tx.push("BuyEvent"); buys += 1; }
                    else if body.starts_with(&DISC_AMM_SELL) { per_tx.push("SellEvent"); sells += 1; }
                }
            }
            if per_tx.len() > 1 { multi += 1; }
            println!("  {}…  events: {:?}", &sig[..16], per_tx);
        }
        println!("\nRAW on-chain: {buys} BuyEvent, {sells} SellEvent, {multi}/{} txs had >1 event",
                 sigs.len());
    }
}

#[cfg(test)]
mod live_direction_truth {
    use super::*;

    /// Prove the buy/sell mapping against ACTUAL token flow.
    ///
    /// Reasoning about base/quote can be talked into either answer; a wallet's
    /// balance cannot. For each event this compares the trader's pre/post token
    /// balance: up = they bought the coin, down = they sold it. If our label
    /// disagrees, buys and sells are inverted — and so would the trades be.
    ///   cargo test --features solana live_direction_truth -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn live_direction_truth() {
        let reg = crate::config::Registry::load("deployments.json").expect("registry");
        let net = reg.networks.iter().find(|n| n.kind.is_solana()).expect("solana net");
        let rpc = Rpc::new_pool(net.rpc_pool());
        let mint: Pubkey = std::env::var("MINT")
            .unwrap_or_else(|_| "DtSA9ReBXyvJqtoJNKNnLToUGgjxjnypwY1o5y7aBX8o".into())
            .parse()
            .expect("valid mint");
        let (pool, p, _) = super::super::pumpswap::find_pool(&rpc, &mint).await.expect("pool");
        let sol_is_base = p.is_sol_based();
        println!("pool {pool}  sol_is_base={sol_is_base}\n");

        let sigs = rpc.signatures_for(&pool, 8).await.unwrap_or_default();
        let (mut agree, mut disagree) = (0, 0);
        for sig in &sigs {
            let Ok(tx) = rpc.transaction(sig).await else { continue };
            let rows = {
                let mut skip = std::collections::HashSet::new();
                let b = amm_tape(&rpc, &pool, 8, &Pubkey::new_unique(), 6, sol_is_base, 1e9, &mut skip).await;
                b.rows.into_iter().filter(|r| &r.signature == sig).collect::<Vec<_>>()
            };
            // Net change in the coin, per owner, straight from the ledger.
            let meta = tx.get("meta");
            let bal = |field: &str| -> Vec<(String, f64)> {
                meta.and_then(|m| m.get(field)).and_then(|b| b.as_array()).map(|a| {
                    a.iter().filter(|e| e.get("mint").and_then(|m| m.as_str()) == Some(&mint.to_string()))
                     .filter_map(|e| Some((
                        e.get("owner")?.as_str()?.to_string(),
                        e.get("uiTokenAmount")?.get("uiAmount")?.as_f64().unwrap_or(0.0))))
                     .collect()
                }).unwrap_or_default()
            };
            let (pre, post) = (bal("preTokenBalances"), bal("postTokenBalances"));
            for r in rows {
                let owner = r.user.to_string();
                let p0: f64 = pre.iter().filter(|(o, _)| *o == owner).map(|(_, v)| *v).sum();
                let p1: f64 = post.iter().filter(|(o, _)| *o == owner).map(|(_, v)| *v).sum();
                let delta = p1 - p0;
                if delta == 0.0 { continue; }
                let truth = if delta > 0.0 { SwapKind::Buy } else { SwapKind::Sell };
                let ok = truth == r.kind;
                if ok { agree += 1 } else { disagree += 1 }
                println!("  {} we={:<4} ledger={:<4} delta={:+.0} {}",
                    &sig[..12], r.kind.label(), truth.label(), delta,
                    if ok { "OK" } else { "!! INVERTED" });
            }
        }
        println!("\n{agree} agree, {disagree} disagree with the ledger");
        assert!(disagree == 0 && agree > 0, "buy/sell labels must match actual token flow");
    }
}

#[cfg(test)]
mod live_curve_tape {
    use super::*;

    /// Decode a NAMED curve coin's tape and print the raw event fields.
    ///   MINT=<base58> cargo test --features solana live_curve_tape_for_mint -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn live_curve_tape_for_mint() {
        let reg = crate::config::Registry::load("deployments.json").expect("registry");
        let net = reg.networks.iter().find(|n| n.kind.is_solana()).expect("solana net");
        let rpc = Rpc::new_pool(net.rpc_pool());
        let mint: Pubkey = std::env::var("MINT").expect("set MINT").parse().expect("valid mint");
        let curve = bonding_curve_pda(&mint);
        println!("curve {curve}");

        let sigs = rpc.signatures_for(&curve, 4).await.unwrap_or_default();
        let pump = PUMP_PROGRAM.to_string();
        for sig in &sigs {
            let Ok(tx) = rpc.transaction(sig).await else { continue };
            let keys = all_account_keys(&tx);
            for group in tx.get("meta").and_then(|m| m.get("innerInstructions"))
                .and_then(|i| i.as_array()).cloned().unwrap_or_default() {
                for ix in group.get("instructions").and_then(|i| i.as_array()).into_iter().flatten() {
                    let pi = ix.get("programIdIndex").and_then(|p| p.as_u64()).unwrap_or(u64::MAX) as usize;
                    if keys.get(pi).map(String::as_str) != Some(pump.as_str()) { continue; }
                    let Some(d) = ix.get("data").and_then(|x| x.as_str()).and_then(b58_decode) else { continue };
                    if d.len() < 16 || d[..8] != DISC_CPI_EVENT { continue; }
                    println!("  event disc {:?} len={}", &d[8..16], d.len());
                    if let Some(ev) = decode_trade_event(&d) {
                        println!("    mint={} sol_amount={} token_amount={} is_buy={}",
                                 ev.mint, ev.sol_amount, ev.token_amount, ev.is_buy);
                        println!("    LEGACY sol_amount={} virt_sol={} real_sol={}",
                                 ev.sol_amount, ev.virtual_sol_reserves, ev.real_sol_reserves);
                        println!("    RESOLVED quote_in={} ({:.6} SOL)  real_quote={} ({:.4} SOL)",
                                 ev.quote_in(), super::super::lamports_to_sol(ev.quote_in()),
                                 ev.real_quote(), super::super::lamports_to_sol(ev.real_quote()));
                        println!("    matches requested mint: {}", ev.mint == mint);
                    } else {
                        println!("    (not a TradeEvent for us)");
                    }
                }
            }
        }
    }
}
