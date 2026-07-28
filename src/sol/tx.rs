// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs
//! Transaction assembly: instructions → signed wire bytes → sent → confirmed.
//!
//! Compute-budget instructions are hand-encoded rather than pulling another
//! crate — they're two tiny instructions and the program is stable.
//!
//! On Solana the fee market IS the priority queue: a transaction without a
//! priority fee can sit unlanded for a long time when blocks are full. Every
//! trade here carries an explicit CU limit + price, so sizing is deliberate
//! rather than accidental.

use solana_hash::Hash;
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::Transaction;

use super::rpc::Rpc;

/// The Compute Budget program (fixed address).
pub const COMPUTE_BUDGET_PROGRAM: Pubkey =
    solana_pubkey::pubkey!("ComputeBudget111111111111111111111111111111");

/// Compute-unit budget for a bonding-curve buy/sell. pump.fun's own client uses
/// 120k for bonding trades; we add headroom for the idempotent ATA create that
/// rides along with a first buy, and for v2's 27-account list (the legacy
/// instructions took 16).
pub const CU_LIMIT_TRADE: u32 = 200_000;
/// AMM swaps touch 21-23 accounts (vs 14-16 on the curve), may create two
/// ATAs, and now carry the SOL wrap (transfer + SyncNative) and the closing
/// unwrap in the same transaction. pump's client uses 200k for the swap alone.
pub const CU_LIMIT_AMM: u32 = 300_000;

/// `SetComputeUnitLimit` — instruction tag 2, then the limit as u32 LE.
pub fn set_cu_limit(units: u32) -> Instruction {
    let mut data = Vec::with_capacity(5);
    data.push(2);
    data.extend_from_slice(&units.to_le_bytes());
    Instruction { program_id: COMPUTE_BUDGET_PROGRAM, accounts: vec![], data }
}

/// `SetComputeUnitPrice` — instruction tag 3, then micro-lamports per CU as u64 LE.
/// Total priority fee paid = `micro_lamports * cu_limit / 1_000_000` lamports.
pub fn set_cu_price(micro_lamports: u64) -> Instruction {
    let mut data = Vec::with_capacity(9);
    data.push(3);
    data.extend_from_slice(&micro_lamports.to_le_bytes());
    Instruction { program_id: COMPUTE_BUDGET_PROGRAM, accounts: vec![], data }
}

/// What a priority setting actually costs, in SOL — so the UI can show the real
/// number instead of an opaque micro-lamport figure.
pub fn priority_fee_sol(micro_lamports: u64, cu_limit: u32) -> f64 {
    let lamports = (micro_lamports as u128 * cu_limit as u128) / 1_000_000;
    super::lamports_to_sol(lamports as u64)
}

/// Build a signed transaction: compute-budget instructions are prepended, then
/// `ixs`, all signed by `signer` against a fresh blockhash.
pub async fn build_signed(
    rpc: &Rpc,
    signer: &Keypair,
    ixs: Vec<Instruction>,
    cu_limit: u32,
    cu_price_micro: u64,
) -> eyre::Result<(Transaction, String)> {
    let blockhash_str = rpc.latest_blockhash().await?;
    let blockhash: Hash = blockhash_str
        .parse()
        .map_err(|_| eyre::eyre!("bad blockhash from rpc: {blockhash_str}"))?;

    let mut all = Vec::with_capacity(ixs.len() + 2);
    all.push(set_cu_limit(cu_limit));
    if cu_price_micro > 0 {
        all.push(set_cu_price(cu_price_micro));
    }
    all.extend(ixs);

    let tx = Transaction::new_signed_with_payer(&all, Some(&signer.pubkey()), &[signer], blockhash);
    Ok((tx, blockhash_str))
}

/// Serialize to the wire format the RPC expects.
pub fn wire(tx: &Transaction) -> eyre::Result<Vec<u8>> {
    bincode::serialize(tx).map_err(|e| eyre::eyre!("transaction serialize: {e}"))
}

/// Build, sign and submit. Returns the signature — submission only, NOT
/// confirmation; poll `confirm` for that.
pub async fn send(
    rpc: &Rpc,
    signer: &Keypair,
    ixs: Vec<Instruction>,
    cu_limit: u32,
    cu_price_micro: u64,
) -> eyre::Result<String> {
    let (tx, _) = build_signed(rpc, signer, ixs, cu_limit, cu_price_micro).await?;
    rpc.send_transaction(&wire(&tx)?).await
}

/// Poll until the signature lands or `timeout` elapses.
/// `Ok(true)` = landed and succeeded, `Ok(false)` = landed but reverted,
/// `Err` = never landed in time (blockhash may still be valid, so it *could*
/// land later — treat as unknown, not as failed).
pub async fn confirm(rpc: &Rpc, sig: &str, timeout: std::time::Duration) -> eyre::Result<bool> {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if let Ok(Some(ok)) = rpc.signature_ok(sig).await {
            return Ok(ok);
        }
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    }
    eyre::bail!("timed out waiting for {sig} to confirm")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_budget_encoding_matches_program_abi() {
        let l = set_cu_limit(150_000);
        assert_eq!(l.program_id, COMPUTE_BUDGET_PROGRAM);
        assert_eq!(l.data[0], 2, "SetComputeUnitLimit is tag 2");
        assert_eq!(&l.data[1..], &150_000u32.to_le_bytes());
        assert!(l.accounts.is_empty(), "compute budget ixs take no accounts");

        let p = set_cu_price(1_000);
        assert_eq!(p.data[0], 3, "SetComputeUnitPrice is tag 3");
        assert_eq!(&p.data[1..], &1_000u64.to_le_bytes());
    }

    #[test]
    fn priority_fee_math_is_right() {
        // 1_000_000 micro-lamports/CU over 150k CU = exactly 150_000 lamports.
        assert_eq!(priority_fee_sol(1_000_000, 150_000), super::super::lamports_to_sol(150_000));
        // A typical setting stays tiny — worth showing so it isn't over-set.
        let f = priority_fee_sol(10_000, 150_000);
        assert!(f > 0.0 && f < 0.01, "unexpected priority fee {f} SOL");
        assert_eq!(priority_fee_sol(0, 150_000), 0.0);
    }

    #[test]
    fn signed_transaction_roundtrips_to_wire() {
        // A self-transfer-free tx (compute budget only) is enough to prove the
        // sign + serialize path produces something the RPC would accept.
        let kp = Keypair::new();
        let tx = Transaction::new_signed_with_payer(
            &[set_cu_limit(1_000)],
            Some(&kp.pubkey()),
            &[&kp],
            Hash::default(),
        );
        let bytes = wire(&tx).expect("serialize");
        assert!(!bytes.is_empty());
        // Wire format starts with a compact-u16 signature count; one signer = 1.
        assert_eq!(bytes[0], 1, "expected exactly one signature");
        assert_eq!(tx.signatures.len(), 1);
    }
}
