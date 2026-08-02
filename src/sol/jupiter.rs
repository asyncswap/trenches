// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! Swapping between the assets that are not memecoins — USDC and SOL.
//!
//! Everything else this app trades lives on pump.fun or PumpSwap, and it builds
//! those instructions itself. USDC/SOL does not: that liquidity is on Orca,
//! Raydium and Meteora, and routing across them is a different problem from
//! swapping against one known pool.
//!
//! So this asks Jupiter for a route and submits what it returns.
//!
//! WHAT THAT COSTS, stated plainly. Everywhere else the app assembles its own
//! instructions and knows exactly what it is signing. Here a remote service
//! builds the transaction and we sign it, so a compromised or hostile response
//! is a transaction against our own wallet. Three things narrow that:
//!
//!   - the fee payer must be us, checked before anything is signed;
//!   - the route's minimum output is enforced ON-CHAIN by the swap program, so
//!     a route cannot quietly deliver less than was quoted;
//!   - it is SIMULATED first, and a simulation that errors is never sent.
//!
//! None of that makes it equivalent to building the instruction ourselves. It
//! is the trade being made, and it is worth knowing it is being made.

use serde_json::{json, Value};
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_transaction::Transaction;

use super::rpc::Rpc;

/// USDC on Solana mainnet.
pub const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
/// Wrapped SOL. Jupiter unwraps it for us when `wrapAndUnwrapSol` is set, so a
/// swap out of USDC lands as native SOL rather than as a token account.
pub const WSOL: &str = "So11111111111111111111111111111111111111112";

fn api() -> String {
    std::env::var("TRENCHES_JUPITER_API")
        .unwrap_or_else(|_| "https://quote-api.jup.ag/v6".to_string())
}

/// What a swap would give, before committing to it.
pub struct Quote {
    /// The whole quote object, passed back to `/swap` unchanged — Jupiter
    /// requires the exact response it produced, not a reconstruction of it.
    raw: Value,
    /// Base units in and out, for showing the trade before it is made.
    pub in_amount: u64,
    pub out_amount: u64,
    /// The least this can deliver — `outAmount` after slippage, and the number
    /// the swap program reverts below.
    ///
    /// This is the one figure in the quote that is a PROMISE rather than an
    /// estimate. `out_amount` is what the route expects at this instant and
    /// nothing enforces it; by the time the transaction lands the pools have
    /// moved. Confirming against the estimate would be agreeing to a number
    /// that cannot be held to.
    pub min_out: u64,
}

/// Ask what `amount` base units of `in_mint` would fetch in `out_mint`.
///
/// `slippage_bps` is enforced on-chain: the route carries a minimum output and
/// the swap program reverts below it, so this is a real floor rather than a
/// hint to the router.
pub async fn quote(
    in_mint: &str,
    out_mint: &str,
    amount: u64,
    slippage_bps: u32,
) -> eyre::Result<Quote> {
    let url = format!(
        "{}/quote?inputMint={in_mint}&outputMint={out_mint}&amount={amount}&slippageBps={slippage_bps}",
        api()
    );
    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        reqwest::Client::new().get(&url).header("accept", "application/json").send(),
    )
    .await
    .map_err(|_| eyre::eyre!("Jupiter did not answer in time"))??;
    if !resp.status().is_success() {
        eyre::bail!("Jupiter refused the quote ({})", resp.status());
    }
    let raw: Value = resp.json().await?;
    // A route that does not exist comes back as an error object rather than an
    // HTTP failure, so the absence of amounts is the real test.
    let num = |k: &str| raw.get(k).and_then(|v| v.as_str()).and_then(|s| s.parse::<u64>().ok());
    let (Some(in_amount), Some(out_amount)) = (num("inAmount"), num("outAmount")) else {
        eyre::bail!("no route for that pair and size");
    };
    // Jupiter calls the enforced floor `otherAmountThreshold`. If it is ever
    // missing, deriving it from the slippage we asked for is right: assuming
    // the floor equals the estimate would show a guarantee nobody made.
    let min_out =
        num("otherAmountThreshold").unwrap_or_else(|| floor_from_slippage(out_amount, slippage_bps));
    Ok(Quote { raw, in_amount, out_amount, min_out })
}

/// Turn a quote into a signed, simulated, submitted swap. Returns the
/// signature.
pub async fn execute(rpc: &Rpc, signer: &Keypair, q: &Quote) -> eyre::Result<String> {
    let me = signer.pubkey();
    let body = json!({
        "quoteResponse": q.raw,
        "userPublicKey": me.to_string(),
        // Native SOL in and out, rather than leaving a wrapped-SOL account
        // behind for the user to find and unwrap themselves.
        "wrapAndUnwrapSol": true,
        // Legacy, not versioned: this app signs `Transaction`, and a versioned
        // one would need address-lookup-table resolution to even read. Jupiter
        // may route slightly worse for it; being able to inspect what we sign
        // is worth more than the last basis point.
        "asLegacyTransaction": true,
    });
    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        reqwest::Client::new().post(format!("{}/swap", api())).json(&body).send(),
    )
    .await
    .map_err(|_| eyre::eyre!("Jupiter did not answer in time"))??;
    if !resp.status().is_success() {
        eyre::bail!("Jupiter refused to build the swap ({})", resp.status());
    }
    let v: Value = resp.json().await?;
    let b64 = v
        .get("swapTransaction")
        .and_then(|s| s.as_str())
        .ok_or_else(|| eyre::eyre!("Jupiter returned no transaction"))?;
    let bytes = base64_decode(b64)?;
    let mut tx: Transaction =
        bincode::deserialize(&bytes).map_err(|e| eyre::eyre!("could not read that transaction: {e}"))?;

    // The one check worth making before a key touches it: WE pay, so we are
    // the account whose balance can move. A transaction with someone else's
    // fee payer is not ours to sign, whatever else it contains.
    let payer = tx
        .message
        .account_keys
        .first()
        .ok_or_else(|| eyre::eyre!("transaction has no accounts"))?;
    if *payer != me {
        eyre::bail!("that transaction pays from {payer}, not from this wallet");
    }

    // Re-sign against a blockhash we fetched. Jupiter's may already be old by
    // the time it reaches us, and we are the only signer.
    let blockhash_str = rpc.latest_blockhash().await?;
    let blockhash = blockhash_str
        .parse()
        .map_err(|_| eyre::eyre!("bad blockhash from rpc: {blockhash_str}"))?;
    tx.message.recent_blockhash = blockhash;
    tx.signatures.clear();
    tx.sign(&[signer], blockhash);

    // Simulated before it is sent. A route that cannot execute — stale, drained
    // mid-quote, or wrong — fails here for free instead of on-chain for gas.
    let wire = super::tx::wire(&tx)?;
    if let Some(err) = rpc.simulate_err(&wire).await? {
        eyre::bail!("the swap would fail: {err}");
    }
    rpc.send_transaction(&wire).await
}

/// Base64 without pulling in a crate for it — the same shape the RPC layer
/// already uses for account data.
fn base64_decode(s: &str) -> eyre::Result<Vec<u8>> {
    super::rpc::b64_decode(s).map_err(|e| eyre::eyre!("bad base64 from Jupiter: {e}"))
}

/// The worst acceptable output for a quote, from the slippage that was asked
/// for. Only used when a response omits the threshold.
///
/// In u128 deliberately: a 9-decimal mint puts whole SOL amounts within a few
/// orders of magnitude of `u64::MAX` once multiplied by 10_000, and a swap that
/// silently wrapped would compute a floor of nearly zero — which is exactly the
/// number that stops protecting you.
fn floor_from_slippage(out: u64, bps: u32) -> u64 {
    let keep = 10_000u128.saturating_sub(bps.min(10_000) as u128);
    (out as u128 * keep / 10_000) as u64
}

/// Whole tokens from base units, for display.
pub fn ui_amount(base: u64, decimals: u32) -> f64 {
    base as f64 / 10f64.powi(decimals as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mints_are_the_canonical_ones() {
        // Typos here would route a swap into a token nobody meant to hold, and
        // both are famous enough to check by eye.
        assert_eq!(USDC, "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
        assert_eq!(WSOL, "So11111111111111111111111111111111111111112");
    }

    /// The floor is what the confirmation promises and what the chain enforces,
    /// so it must come out BELOW the estimate — never equal to it.
    #[test]
    fn the_floor_sits_under_the_estimate_by_the_slippage() {
        assert_eq!(floor_from_slippage(1_000_000, 50), 995_000); // 0.5%
        assert_eq!(floor_from_slippage(1_000_000, 100), 990_000); // 1%
        assert!(floor_from_slippage(1_000_000, 1) < 1_000_000, "never equal to the estimate");
    }

    /// A whole-SOL amount times 10_000 overflows u64. Wrapping there would
    /// produce a floor near zero and call it protection.
    #[test]
    fn a_large_amount_does_not_wrap_the_floor_to_nothing() {
        let huge = u64::MAX / 2; // far past any real balance, and past u64/10_000
        let floor = floor_from_slippage(huge, 100);
        assert!(floor > huge / 2, "the floor stayed proportional: {floor}");
        assert!(floor < huge);
    }

    /// Nonsense slippage must clamp to "expect nothing" rather than underflow
    /// into a floor larger than the trade.
    #[test]
    fn absurd_slippage_floors_at_zero() {
        assert_eq!(floor_from_slippage(1_000_000, 10_000), 0);
        assert_eq!(floor_from_slippage(1_000_000, 99_999), 0);
    }

    #[test]
    fn base_units_convert_the_way_each_mint_counts() {
        // USDC is 6-dec and SOL is 9. Reading either at the other's scale is
        // off by a thousand, in the direction that spends more than intended.
        assert!((ui_amount(1_500_000, 6) - 1.5).abs() < 1e-9);
        assert!((ui_amount(1_500_000_000, 9) - 1.5).abs() < 1e-9);
    }
}
