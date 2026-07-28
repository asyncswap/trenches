// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs
//! Solana / pump.fun chain adapter. Behind the `solana` cargo feature so the
//! EVM-only build stays fast (the solana crate tree is heavy).
//!
//! Layout mirrors the EVM side: `wallet` (keys), `rpc` (JSON-RPC over the same
//! reqwest client the EVM batch calls use), `pumpfun` (PDAs, bonding-curve
//! state, buy/sell instruction building). Everything renders through the shared
//! chain-agnostic `view` models, so the dashboard is the same code.
//!
//! Addresses and layouts here are taken from the official IDL in
//! `../pump-public-docs/idl/pump.json` — not guessed.
//!
//! Some constants (the AMM program, Token-2022, NATIVE_MINT) aren't referenced
//! yet — they belong to the post-graduation AMM path. They're kept here so the
//! verified addresses live in one place rather than being re-derived later.
#![allow(dead_code)]

pub mod app;
pub mod discover;
pub mod engine;
pub mod metadata;
pub mod pumpfun;
pub mod pumpswap;
pub mod rpc;
pub mod rugcheck;
pub mod trade;
pub mod tx;
pub mod wallet;

use solana_pubkey::Pubkey;

// ---- session trace -------------------------------------------------------

/// Diagnostic trace for a Solana session.
///
/// Deliberately SEPARATE from the trading log: that one records what the bot
/// did (fills, PnL) and is worth keeping; this one records how the machinery
/// behaved (websocket handshakes, decode misses, RPC errors, poll timings) and
/// exists to answer "why is nothing on screen". A quiet feed and a dead feed
/// look identical in the UI — only this tells them apart.
///
/// Writes to `.bot/sol-trace-<start>.log`. Never panics and never blocks the
/// caller on failure: tracing must not be able to take the bot down.
pub fn trace(msg: &str) {
    use std::io::Write;
    use std::sync::{Mutex, OnceLock};

    static FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
    static START: OnceLock<std::time::Instant> = OnceLock::new();

    let start = START.get_or_init(std::time::Instant::now);
    let f = FILE.get_or_init(|| {
        std::fs::create_dir_all(crate::state_dir()).ok()?;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(format!("{}/sol-trace-{ts}.log", crate::state_dir()))
            .ok()
            .map(Mutex::new)
    });
    if let Some(f) = f {
        // Both clocks. Elapsed answers "nothing happened for a minute"; the wall
        // clock is what lets a trace line be lined up against the session log,
        // which is the only way to see what the bot was doing around a fill.
        if let Ok(mut f) = f.lock() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let (h, m, s) = ((now % 86400) / 3600, (now % 3600) / 60, now % 60);
            let _ = writeln!(
                f,
                "[{h:02}:{m:02}:{s:02}] {:8.3}  {msg}",
                start.elapsed().as_secs_f64()
            );
            let _ = f.flush();
        }
    }
}

/// Append a line to the session log, the durable record of what was done and
/// when.
///
/// The trace log next to it is for diagnosing a run in progress, so it counts
/// seconds since start. That is the wrong clock for "when did I buy this" — the
/// question the fill history asks, and one nobody can answer from an elapsed
/// count once the process is gone. This is the EVM side's `session-<ts>.log`,
/// same name and same `[HH:MM:SS] ` prefix, so one parser reads both chains.
pub fn session(line: &str) {
    use std::io::Write;
    use std::sync::{Mutex, OnceLock};

    static FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
    let f = FILE.get_or_init(|| {
        std::fs::create_dir_all(crate::state_dir()).ok()?;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(format!("{}/session-{ts}.log", crate::state_dir()))
            .ok()
            .map(Mutex::new)
    });
    if let Some(f) = f {
        if let Ok(mut f) = f.lock() {
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }
}

/// Pump bonding-curve program.
pub const PUMP_PROGRAM: Pubkey = solana_pubkey::pubkey!("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P");
/// Pump swap AMM (post-graduation pools).
pub const PUMP_AMM_PROGRAM: Pubkey = solana_pubkey::pubkey!("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA");
/// Pump fees program (holds `fee_config`).
pub const PUMP_FEES_PROGRAM: Pubkey = solana_pubkey::pubkey!("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ");

/// Wrapped SOL — the quote mint for SOL-paired coins.
pub const NATIVE_MINT: Pubkey = solana_pubkey::pubkey!("So11111111111111111111111111111111111111112");
pub const TOKEN_PROGRAM: Pubkey = solana_pubkey::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub const TOKEN_2022_PROGRAM: Pubkey = solana_pubkey::pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
pub const ATA_PROGRAM: Pubkey = solana_pubkey::pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
pub const SYSTEM_PROGRAM: Pubkey = solana_pubkey::pubkey!("11111111111111111111111111111111");

/// SOL has 9 decimals; pump.fun coins have 6. Mixing these up is the classic
/// way to send 1000x the intended size, so they're named constants.
pub const SOL_DECIMALS: u32 = 9;
pub const TOKEN_DECIMALS: u32 = 6;
pub const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

/// Lamports -> SOL.
pub fn lamports_to_sol(l: u64) -> f64 {
    l as f64 / LAMPORTS_PER_SOL as f64
}
/// SOL -> lamports (saturating, never negative).
pub fn sol_to_lamports(s: f64) -> u64 {
    if s <= 0.0 { 0 } else { (s * LAMPORTS_PER_SOL as f64) as u64 }
}
/// Base units -> whole tokens (6 dp).
pub fn units_to_tokens(u: u64) -> f64 {
    u as f64 / 10f64.powi(TOKEN_DECIMALS as i32)
}
/// Whole tokens -> base units (6 dp).
pub fn tokens_to_units(t: f64) -> u64 {
    if t <= 0.0 { 0 } else { (t * 10f64.powi(TOKEN_DECIMALS as i32)) as u64 }
}

// ---- PDAs (seeds straight from the IDL) ----------------------------------

/// `["global"]` — global config (holds the fee-recipient list).
pub fn global_pda() -> Pubkey {
    Pubkey::find_program_address(&[b"global"], &PUMP_PROGRAM).0
}

/// `["bonding-curve", mint]` — the coin's curve account (price + reserves).
pub fn bonding_curve_pda(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"bonding-curve", mint.as_ref()], &PUMP_PROGRAM).0
}

/// `["creator-vault", creator]` — where the creator's fees accrue.
pub fn creator_vault_pda(creator: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"creator-vault", creator.as_ref()], &PUMP_PROGRAM).0
}

/// `["__event_authority"]` — CPI event authority.
pub fn event_authority_pda() -> Pubkey {
    Pubkey::find_program_address(&[b"__event_authority"], &PUMP_PROGRAM).0
}

/// `["global_volume_accumulator"]`.
pub fn global_volume_accumulator_pda() -> Pubkey {
    Pubkey::find_program_address(&[b"global_volume_accumulator"], &PUMP_PROGRAM).0
}

/// `["user_volume_accumulator", user]` — created on first buy if missing.
pub fn user_volume_accumulator_pda(user: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"user_volume_accumulator", user.as_ref()], &PUMP_PROGRAM).0
}

/// `["sharing-config", mint]` — like `fee_config`, this lives under the FEES
/// program rather than pump itself. Required by the v2 trade instructions.
pub fn sharing_config_pda(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"sharing-config", mint.as_ref()], &PUMP_FEES_PROGRAM).0
}

/// `["fee_config", pump_program]` — lives under the FEES program, not pump.
pub fn fee_config_pda() -> Pubkey {
    Pubkey::find_program_address(&[b"fee_config", PUMP_PROGRAM.as_ref()], &PUMP_FEES_PROGRAM).0
}

/// Associated token account: `[owner, token_program, mint]` under the ATA
/// program. `token_program` must be the mint's actual owner (SPL vs Token-2022)
/// — never assume, read it from the mint account.
pub fn ata(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ATA_PROGRAM,
    )
    .0
}

#[cfg(test)]
mod pda_tests {
    use super::*;

    /// Prints the addresses our derivation produces, so they can be checked
    /// against real mainnet accounts (see the verification notes in
    /// solana-pumpfun-port memory). Run with `--nocapture`.
    #[test]
    fn print_derived_pdas() {
        println!("global                     = {}", global_pda());
        println!("event_authority            = {}", event_authority_pda());
        println!("global_volume_accumulator  = {}", global_volume_accumulator_pda());
        println!("fee_config                 = {}", fee_config_pda());
    }
}
