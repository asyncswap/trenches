// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! Which tokens has this wallet EVER received?
//!
//! No EVM RPC answers that directly — a wallet's token list is discovered,
//! not queried. The primitive is a `Transfer(_, to, _)` log scan filtered on
//! the recipient topic: one pass over history finds every token that ever
//! landed here, and a cursor on disk makes every later pass incremental, so
//! the cost is paid once per wallet rather than once per look.
//!
//! The first pass reaches back a bounded window rather than to genesis: at
//! ~10 blocks a second, two million blocks is a bit over two days, which
//! covers "what did I buy lately" without spending an afternoon of quota on
//! a chain's whole life. Anything older that still matters is in the pool
//! registry or the basis files anyway.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use alloy::primitives::{b256, Address, B256};
use alloy::providers::Provider;
use alloy::rpc::types::Filter;

const TRANSFER: B256 = b256!("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");
const LOOKBACK: u64 = 2_000_000;
/// The widest range the strictest upstream allows. Anything larger 413s with
/// "eth_getLogs is limited to a 10,000 range" and the scan starves.
const CHUNK: u64 = 10_000;

#[derive(serde::Serialize, serde::Deserialize, Default, Clone)]
struct SavedTokens {
    scanned_to: u64,
    tokens: Vec<Address>,
}

fn path(trader: Address) -> String {
    format!("{}/wallet-tokens-{}-{trader:#x}.json", crate::state_dir(), crate::chain_id())
}

fn cache() -> &'static Mutex<Option<SavedTokens>> {
    static C: std::sync::OnceLock<Mutex<Option<SavedTokens>>> = std::sync::OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

fn load(trader: Address) -> SavedTokens {
    if let Some(s) = crate::lock(cache()).clone() {
        return s;
    }
    let s: SavedTokens = std::fs::read_to_string(path(trader))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    *crate::lock(cache()) = Some(s.clone());
    s
}

fn save(trader: Address, s: &SavedTokens) {
    *crate::lock(cache()) = Some(s.clone());
    let _ = std::fs::write(path(trader), serde_json::to_string(s).unwrap_or_default());
}

/// Every token this wallet has ever been seen receiving. Instant: disk/memory.
pub fn known_tokens(trader: Address) -> Vec<Address> {
    load(trader).tokens
}

/// Bring the scan up to the head, in the background. Idempotent: one scan at
/// a time, and a call while one runs is a no-op.
pub fn ensure_scan<P: Provider + Clone + Send + Sync + 'static>(provider: &P, trader: Address) {
    static RUNNING: AtomicBool = AtomicBool::new(false);
    if trader.is_zero() || RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    let p = provider.clone();
    tokio::spawn(async move {
        let done = scan(&p, trader).await;
        RUNNING.store(false, Ordering::SeqCst);
        if let Some(n) = done {
            crate::trace(&format!("wallet scan: up to date, {n} token(s) known"));
        }
    });
}

async fn scan<P: Provider>(provider: &P, trader: Address) -> Option<usize> {
    let head = provider.get_block_number().await.ok()?;
    let mut saved = load(trader);
    let mut from = if saved.scanned_to > 0 {
        saved.scanned_to + 1
    } else {
        head.saturating_sub(LOOKBACK)
    };
    if from > head {
        return Some(saved.tokens.len());
    }
    let mut seen: HashSet<Address> = saved.tokens.iter().copied().collect();
    let total = head.saturating_sub(from);
    if total > CHUNK {
        crate::trace(&format!("wallet scan: {total} blocks to cover, chunked"));
    }
    let mut chunk = CHUNK;
    while from <= head {
        let to = (from + chunk - 1).min(head);
        let filter = Filter::new()
            .event_signature(TRANSFER)
            .topic2(trader.into_word())
            .from_block(from)
            .to_block(to);
        match tokio::time::timeout(std::time::Duration::from_secs(20), provider.get_logs(&filter))
            .await
        {
            Ok(Ok(logs)) => {
                let mut fresh = 0;
                for lg in logs {
                    // Only real ERC-20 transfers: 3 topics (sig, from, to).
                    // An NFT transfer has 4 and would pollute the list.
                    if lg.topics().len() == 3 && seen.insert(lg.address()) {
                        saved.tokens.push(lg.address());
                        fresh += 1;
                    }
                }
                saved.scanned_to = to;
                save(trader, &saved);
                if fresh > 0 {
                    crate::trace(&format!("wallet scan: {from}..{to}: {fresh} new token(s)"));
                }
                from = to + 1;
                // Room for the rest of the app: this is background work on a
                // metered pipe, not a race.
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            }
            Ok(Err(e)) => {
                let msg = e.to_string().to_lowercase();
                // A range cap is a fact about the endpoint, not this range:
                // shrink and retry the same blocks rather than giving up.
                if (msg.contains("range") || msg.contains("limited")) && chunk > 1_000 {
                    chunk /= 2;
                    crate::trace(&format!("wallet scan: range capped, chunk now {chunk}"));
                    continue;
                }
                // Stop rather than skip: the cursor stays where the last
                // success left it, and the next call resumes there. A skipped
                // range is a token that never appears, silently.
                crate::trace(&format!("wallet scan: {from}..{to} failed: {e}"));
                return None;
            }
            Err(_) => {
                crate::trace(&format!("wallet scan: {from}..{to} timed out"));
                return None;
            }
        }
    }
    Some(saved.tokens.len())
}
