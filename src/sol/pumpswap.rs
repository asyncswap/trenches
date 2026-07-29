// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! PumpSwap AMM — where pump.fun coins trade **after** they graduate.
//!
//! Once `BondingCurve.complete` flips, liquidity migrates to this constant-product
//! AMM and the bonding-curve instructions stop working. Most coins with real
//! volume are here, so bonding-curve-only support misses the liquid half of the
//! market entirely.
//!
//! Layouts, discriminators and account ORDER come from the official IDL
//! (`../pump-public-docs/idl/pump_amm.json`) — the same source used for the
//! bonding curve, not guesswork.

use borsh::BorshDeserialize;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;

use super::rpc::Rpc;
use super::{ata, NATIVE_MINT, PUMP_AMM_PROGRAM, PUMP_FEES_PROGRAM, SYSTEM_PROGRAM, ATA_PROGRAM};

/// Anchor discriminators from the AMM IDL.
const DISC_BUY_EXACT_QUOTE_IN: [u8; 8] = [198, 46, 21, 82, 180, 217, 232, 112];
const DISC_SELL: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];
const DISC_DEPOSIT: [u8; 8] = [242, 35, 198, 137, 82, 225, 242, 182];
const DISC_WITHDRAW: [u8; 8] = [183, 18, 70, 156, 148, 109, 161, 34];
pub const DISC_POOL: [u8; 8] = [241, 154, 109, 4, 17, 177, 109, 188];
const DISC_GLOBAL_CONFIG: [u8; 8] = [149, 8, 156, 202, 160, 252, 176, 217];

/// Byte offset of `quote_mint`, immediately after `base_mint`.
const POOL_QUOTE_MINT_OFFSET: usize = POOL_BASE_MINT_OFFSET + 32;

/// Byte offset of `base_mint` within a `Pool` account. Used to find a coin's
/// pool by `getProgramAccounts` memcmp, since the pool PDA needs a creator+index
/// we don't know for an arbitrary coin.
///
/// Pools are matched by discriminator + this offset, never by account size:
/// mainnet carries both 261-byte (original) and 301-byte (extended) pools, and
/// filtering on 261 hid every newer pool — including graduated coins like ansem.
/// Fields before `lp_supply` keep their offsets across that growth.
const POOL_BASE_MINT_OFFSET: usize = 43;

/// Literal seed the IDL uses for the AMM's `fee_config` PDA.
const FEE_CONFIG_SEED: [u8; 32] = [
    12, 20, 222, 252, 130, 94, 198, 118, 148, 37, 8, 24, 187, 101, 64, 101, 244, 41, 141, 49, 86,
    213, 113, 180, 212, 248, 9, 12, 24, 233, 168, 99,
];

/// A graduated coin's AMM pool.
#[derive(Debug, Clone, BorshDeserialize)]
pub struct Pool {
    pub pool_bump: u8,
    pub index: u16,
    pub creator: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub lp_mint: Pubkey,
    pub pool_base_token_account: Pubkey,
    pub pool_quote_token_account: Pubkey,
    pub lp_supply: u64,
    pub coin_creator: Pubkey,
    pub is_mayhem_mode: bool,
    pub is_cashback_coin: bool,
}

impl Pool {
    pub fn decode(data: &[u8]) -> eyre::Result<Pool> {
        if data.len() < 8 || data[..8] != DISC_POOL {
            eyre::bail!("not a PumpSwap Pool account");
        }
        let mut rest = &data[8..];
        Pool::deserialize(&mut rest).map_err(|e| eyre::eyre!("pool decode: {e}"))
    }

    /// True for the classic orientation: coin as base, SOL as quote.
    pub fn is_sol_quoted(&self) -> bool {
        self.quote_mint == NATIVE_MINT
    }

    /// True when SOL sits in the BASE slot — the inverted orientation, which is
    /// the majority of SOL pools on mainnet.
    pub fn is_sol_based(&self) -> bool {
        self.base_mint == NATIVE_MINT
    }

    /// Tradeable here if exactly one side is SOL. A pool with SOL on both sides
    /// (or neither) has no coin to price.
    pub fn sol_paired(&self) -> bool {
        self.is_sol_quoted() != self.is_sol_based()
    }

    /// BOOST: the pool's virtual quote reserves, raw base units of the quote
    /// mint. Read by OFFSET, not by widening the borsh struct: pools created
    /// before the boost upgrade are shorter, and a strict decode would refuse
    /// to load every one of them. Zero when absent — which is also the value
    /// that makes all the boost math collapse back to the plain formulas.
    ///
    /// Pricing on a boost pool uses effective = real + virtual, but a sell's
    /// PAYOUT is capped at the real vault (the program refuses past it, 6063).
    pub fn virtual_quote_reserves(data: &[u8]) -> i128 {
        const OFF: usize = 8      // discriminator
            + 1 + 2               // pool_bump, index
            + 32 * 6              // creator, base/quote/lp mints, both vault ATAs
            + 8                   // lp_supply
            + 32 + 1 + 1; // coin_creator, is_mayhem_mode, is_cashback_coin
        data.get(OFF..OFF + 16)
            .and_then(|b| b.try_into().ok())
            .map(i128::from_le_bytes)
            .unwrap_or(0)
    }

    /// The non-SOL side — the coin this pool actually trades.
    pub fn coin_mint(&self) -> Option<Pubkey> {
        match (self.is_sol_based(), self.is_sol_quoted()) {
            (true, false) => Some(self.quote_mint),
            (false, true) => Some(self.base_mint),
            _ => None,
        }
    }
}

/// The prefix of `GlobalConfig` we need: the protocol fee recipients.
#[derive(Debug, BorshDeserialize)]
pub struct GlobalConfigHead {
    pub admin: Pubkey,
    pub lp_fee_basis_points: u64,
    pub protocol_fee_basis_points: u64,
    pub disable_flags: u8,
    pub protocol_fee_recipients: [Pubkey; 8],
}

impl GlobalConfigHead {
    pub fn decode(data: &[u8]) -> eyre::Result<GlobalConfigHead> {
        if data.len() < 8 || data[..8] != DISC_GLOBAL_CONFIG {
            eyre::bail!("not a PumpSwap GlobalConfig account");
        }
        let mut rest = &data[8..];
        GlobalConfigHead::deserialize(&mut rest).map_err(|e| eyre::eyre!("global config decode: {e}"))
    }

    /// First non-default fee recipient — any of the eight is valid.
    pub fn fee_recipient(&self) -> Pubkey {
        self.protocol_fee_recipients
            .iter()
            .copied()
            .find(|p| *p != Pubkey::default())
            .unwrap_or_default()
    }

    /// Total swap fee as a fraction (LP + protocol).
    pub fn fee_frac(&self) -> f64 {
        (self.lp_fee_basis_points + self.protocol_fee_basis_points) as f64 / 10_000.0
    }

    /// A buyback fee recipient — any non-default of the eight is valid, same
    /// rule as the protocol recipients. Read by OFFSET (not by widening the
    /// borsh head) so a future appended field cannot break loading every
    /// pool; the offsets were verified against the live GlobalConfig
    /// (recipients at 643, buyback_basis_points 5000 right behind them).
    pub fn buyback_recipient(data: &[u8]) -> Option<Pubkey> {
        const OFF: usize = 8      // discriminator
            + 32 + 8 + 8 + 1      // admin, lp_fee, protocol_fee, disable_flags
            + 32 * 8              // protocol_fee_recipients
            + 8                   // coin_creator_fee_basis_points
            + 32 + 32 + 32 + 1    // admin_set_creator, whitelist, reserved_recipient, mayhem
            + 32 * 7              // reserved_fee_recipients
            + 1; // is_cashback_enabled
        if data.len() < 8 || data[..8] != DISC_GLOBAL_CONFIG {
            return None;
        }
        data.get(OFF..OFF + 32 * 8)?
            .chunks_exact(32)
            .filter_map(|c| Pubkey::try_from(c).ok())
            .find(|p| *p != Pubkey::default())
    }
}

// ---- PDAs ----------------------------------------------------------------

pub fn global_config_pda() -> Pubkey {
    Pubkey::find_program_address(&[b"global_config"], &PUMP_AMM_PROGRAM).0
}
pub fn event_authority_pda() -> Pubkey {
    Pubkey::find_program_address(&[b"__event_authority"], &PUMP_AMM_PROGRAM).0
}
pub fn global_volume_accumulator_pda() -> Pubkey {
    Pubkey::find_program_address(&[b"global_volume_accumulator"], &PUMP_AMM_PROGRAM).0
}
pub fn user_volume_accumulator_pda(user: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"user_volume_accumulator", user.as_ref()], &PUMP_AMM_PROGRAM).0
}
/// `["creator_vault", pool.coin_creator]` — note the UNDERSCORE, unlike the
/// bonding curve's hyphenated `"creator-vault"`. Easy to conflate; they differ.
pub fn coin_creator_vault_authority(coin_creator: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"creator_vault", coin_creator.as_ref()], &PUMP_AMM_PROGRAM).0
}

/// `["pool-v2", base_mint]` — note the HYPHEN, unlike every underscore seed
/// around it. The v2 pool state the swap instructions verify via a remaining
/// account whenever the pool has a coin creator.
pub fn pool_v2_pda(base_mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"pool-v2", base_mint.as_ref()], &PUMP_AMM_PROGRAM).0
}
pub fn fee_config_pda() -> Pubkey {
    Pubkey::find_program_address(&[b"fee_config", &FEE_CONFIG_SEED], &PUMP_FEES_PROGRAM).0
}

// ---- discovery -----------------------------------------------------------

/// Find the SOL-quoted AMM pool for `mint`.
///
/// The pool PDA is seeded with `(index, creator, base_mint, quote_mint)` and we
/// know only the mint, so this filters `getProgramAccounts` by account size plus
/// a memcmp on `base_mint`. Picks the deepest pool when a coin has several.
pub async fn find_pool(rpc: &Rpc, mint: &Pubkey) -> eyre::Result<(Pubkey, Pool, i128)> {
    // Look for the coin on BOTH sides. A PumpSwap pool may be created either way
    // round, and searching only the base slot hid ~78% of SOL pools on mainnet —
    // every one of them reported as "not a pump.fun coin".
    let (as_base, as_quote) = tokio::join!(
        rpc.program_accounts_memcmp(&PUMP_AMM_PROGRAM, &DISC_POOL, POOL_BASE_MINT_OFFSET, mint),
        rpc.program_accounts_memcmp(&PUMP_AMM_PROGRAM, &DISC_POOL, POOL_QUOTE_MINT_OFFSET, mint),
    );
    let mut accounts = as_base?;
    accounts.extend(as_quote?);
    let total = accounts.len();
    let mut candidates: Vec<(Pubkey, Pool, i128)> = Vec::new();
    for (key, data) in accounts {
        let Ok(pool) = Pool::decode(&data) else { continue };
        let virtual_quote = Pool::virtual_quote_reserves(&data);
        // Exactly one side must be SOL, and the other must be this coin.
        if !pool.sol_paired() || pool.coin_mint() != Some(*mint) {
            continue;
        }
        candidates.push((key, pool, virtual_quote));
    }

    // Rank by the SOL actually in the pool, not by `lp_supply`.
    //
    // LP supply is set by whatever the first depositor put in, so it is not
    // comparable between pools: a live coin had a 47-billion-LP pool holding
    // 9.83 SOL beaten by a 6-million-LP pool holding 8,945 SOL. Picking on LP
    // supply routinely selected a dust pool over the real market — a trade
    // there would have moved price enormously against us.
    //
    // Popular coins have >100 pools, so balances are read in ONE batch.
    let vaults: Vec<Pubkey> = candidates
        .iter()
        .map(|(_, p, _)| if p.is_sol_based() { p.pool_base_token_account } else { p.pool_quote_token_account })
        .collect();
    let mut sol: Vec<u64> = Vec::with_capacity(vaults.len());
    for chunk in vaults.chunks(100) {
        match rpc.token_balances(chunk).await {
            Ok(b) => sol.extend(b.into_iter().map(|v| v.unwrap_or(0))),
            // Fall back to LP supply rather than failing outright.
            Err(_) => sol.resize(vaults.len(), 0),
        }
    }
    sol.resize(candidates.len(), 0);

    let mut best: Option<(Pubkey, Pool, i128, u64)> = None;
    for ((key, pool, vq), lamports) in candidates.into_iter().zip(sol) {
        let depth = if lamports > 0 { lamports } else { pool.lp_supply.min(1) };
        if best.as_ref().is_none_or(|(_, _, _, d)| depth > *d) {
            best = Some((key, pool, vq, depth));
        }
    }
    // Say which of the two failures it is. "no SOL-quoted pool" reads as a
    // transient lookup problem when usually the coin simply isn't a pump coin
    // and trades on a DEX this bot doesn't speak.
    if let Some((k, p, vq, _)) = best {
        return Ok((k, p, vq));
    }
    if total == 0 {
        eyre::bail!("not a pump.fun coin — no PumpSwap pool exists for this mint");
    }
    eyre::bail!("this coin has a PumpSwap pool, but it is not paired against SOL")
}

/// Live reserves: `(base tokens, quote SOL)` read from the pool's token accounts.
/// Live reserves as `(SOL, coin)` — by MEANING, not by pool slot, so callers
/// cannot accidentally price an inverted pool upside down.
pub async fn reserves(rpc: &Rpc, pool: &Pool, token_decimals: u8) -> eyre::Result<(f64, f64)> {
    let (base_raw, quote_raw) = tokio::join!(
        rpc.token_balance(&pool.pool_base_token_account),
        rpc.token_balance(&pool.pool_quote_token_account),
    );
    let (sol_raw, tok_raw) = if pool.is_sol_based() {
        (base_raw.unwrap_or(0), quote_raw.unwrap_or(0))
    } else {
        (quote_raw.unwrap_or(0), base_raw.unwrap_or(0))
    };
    Ok((
        super::lamports_to_sol(sol_raw),
        tok_raw as f64 / 10f64.powi(token_decimals as i32),
    ))
}

/// Constant-product quote: SOL in → tokens out, net of `fee_frac`.
pub fn tokens_out(base_res: f64, quote_res: f64, sol_in: f64, fee_frac: f64) -> f64 {
    if sol_in <= 0.0 || base_res <= 0.0 || quote_res <= 0.0 {
        return 0.0;
    }
    let dx = sol_in * (1.0 - fee_frac);
    (base_res * dx / (quote_res + dx)).max(0.0)
}

/// Constant-product quote: tokens in → SOL out, net of `fee_frac`.
pub fn sol_out(base_res: f64, quote_res: f64, tokens_in: f64, fee_frac: f64) -> f64 {
    if tokens_in <= 0.0 || base_res <= 0.0 || quote_res <= 0.0 {
        return 0.0;
    }
    let dy = tokens_in * (1.0 - fee_frac);
    (quote_res * dy / (base_res + dy)).max(0.0)
}

// ---- instructions --------------------------------------------------------

/// Everything needed to address a pool's accounts, resolved once.
#[derive(Debug, Clone, Copy)]
pub struct SwapKeys {
    pub pool: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub base_token_program: Pubkey,
    pub quote_token_program: Pubkey,
    pub pool_base_ta: Pubkey,
    pub pool_quote_ta: Pubkey,
    pub coin_creator: Pubkey,
    pub fee_recipient: Pubkey,
    /// The pool's `is_cashback_coin` flag: decides whether the cashback
    /// remaining accounts ride along (mandatory on such pools, 6059 without).
    pub is_cashback_coin: bool,
    /// A buyback fee recipient from GlobalConfig. The SDK appends the pair on
    /// every swap; None (config unreadable) sends none and lets preflight say
    /// so rather than inventing an address.
    pub buyback_recipient: Option<Pubkey>,
}

/// The 21 accounts shared by both sides, in IDL order. Buy appends two more.
///
/// ⚠️ Order is transcribed from the IDL — do not tidy. Writability is copied
/// exactly for the same reason as the bonding curve: a spurious write lock on a
/// shared account serialises us behind every other trader.
fn base_accounts(k: &SwapKeys, user: &Pubkey) -> Vec<AccountMeta> {
    let creator_vault_auth = coin_creator_vault_authority(&k.coin_creator);
    vec![
        AccountMeta::new(k.pool, false),                                                //  1 pool            W
        AccountMeta::new(*user, true),                                                  //  2 user            W signer
        AccountMeta::new_readonly(global_config_pda(), false),                          //  3 global_config
        AccountMeta::new_readonly(k.base_mint, false),                                  //  4 base_mint
        AccountMeta::new_readonly(k.quote_mint, false),                                 //  5 quote_mint
        AccountMeta::new(ata(user, &k.base_mint, &k.base_token_program), false),        //  6 user_base_ta    W
        AccountMeta::new(ata(user, &k.quote_mint, &k.quote_token_program), false),      //  7 user_quote_ta   W
        AccountMeta::new(k.pool_base_ta, false),                                        //  8 pool_base_ta    W
        AccountMeta::new(k.pool_quote_ta, false),                                       //  9 pool_quote_ta   W
        AccountMeta::new_readonly(k.fee_recipient, false),                              // 10 fee_recipient
        AccountMeta::new(ata(&k.fee_recipient, &k.quote_mint, &k.quote_token_program), false), // 11 fee_recip_ta W
        AccountMeta::new_readonly(k.base_token_program, false),                         // 12 base_token_prog
        AccountMeta::new_readonly(k.quote_token_program, false),                        // 13 quote_token_prog
        AccountMeta::new_readonly(SYSTEM_PROGRAM, false),                               // 14 system_program
        AccountMeta::new_readonly(ATA_PROGRAM, false),                                  // 15 assoc_token_prog
        AccountMeta::new_readonly(event_authority_pda(), false),                        // 16 event_authority
        AccountMeta::new_readonly(PUMP_AMM_PROGRAM, false),                             // 17 program
        AccountMeta::new(ata(&creator_vault_auth, &k.quote_mint, &k.quote_token_program), false), // 18 creator_vault_ata W
        AccountMeta::new_readonly(creator_vault_auth, false),                           // 19 creator_vault_auth
        AccountMeta::new_readonly(fee_config_pda(), false),                             // 20 fee_config
        AccountMeta::new_readonly(PUMP_FEES_PROGRAM, false),                            // 21 fee_program
    ]
}

/// `buy_exact_quote_in`: spend exactly `spendable_quote_in` lamports, requiring
/// at least `min_base_amount_out` token base units back.
///
/// Same shape as the bonding curve's `buy_exact_sol_in` — spend a SOL amount, set
/// a token floor — so the bot's sizing model carries over unchanged.
pub fn buy_ix(
    k: &SwapKeys,
    user: &Pubkey,
    spendable_quote_in: u64,
    min_base_amount_out: u64,
) -> Instruction {
    let mut data = Vec::with_capacity(8 + 8 + 8 + 1);
    data.extend_from_slice(&DISC_BUY_EXACT_QUOTE_IN);
    data.extend_from_slice(&spendable_quote_in.to_le_bytes());
    data.extend_from_slice(&min_base_amount_out.to_le_bytes());
    data.push(1); // track_volume

    // Buy takes the 21 shared accounts, with the volume accumulators spliced in
    // at 20-21 (pushing fee_config/fee_program to 22-23) per the IDL.
    let mut accounts = base_accounts(k, user);
    let tail = accounts.split_off(19); // fee_config, fee_program
    accounts.push(AccountMeta::new_readonly(global_volume_accumulator_pda(), false)); // 20
    accounts.push(AccountMeta::new(user_volume_accumulator_pda(user), false));        // 21
    accounts.extend(tail);                                                            // 22, 23
    accounts.extend(remaining_accounts(k, user, false));

    Instruction { program_id: PUMP_AMM_PROGRAM, accounts, data }
}

/// The quote-mint (WSOL) associated token account of the user's volume
/// accumulator for the Pump AMM program — where AMM cashback accrues.
fn uva_quote_ata(k: &SwapKeys, user: &Pubkey) -> Pubkey {
    ata(&user_volume_accumulator_pda(user), &k.quote_mint, &k.quote_token_program)
}

/// The remaining accounts both swap sides carry, in the SDK's exact order —
/// the program addresses them by position, so order IS correctness:
///   1. cashback coins: the accumulator's quote ATA (and on sells, the
///      accumulator itself) — 6059 without them;
///   2. pools with a coin creator: the `pool_v2` PDA — 6062 without it
///      ("pool_v2 remaining account is missing or invalid", which is how a
///      live ansem buy failed);
///   3. always: a buyback fee recipient and its quote ATA.
fn remaining_accounts(k: &SwapKeys, user: &Pubkey, sell: bool) -> Vec<AccountMeta> {
    let mut v = Vec::new();
    if k.is_cashback_coin {
        v.push(AccountMeta::new(uva_quote_ata(k, user), false));
        if sell {
            v.push(AccountMeta::new(user_volume_accumulator_pda(user), false));
        }
    }
    if k.coin_creator != Pubkey::default() {
        v.push(AccountMeta::new_readonly(pool_v2_pda(&k.base_mint), false));
    }
    if let Some(b) = k.buyback_recipient {
        v.push(AccountMeta::new_readonly(b, false));
        v.push(AccountMeta::new(ata(&b, &k.quote_mint, &k.quote_token_program), false));
    }
    v
}

/// `sell`: sell `base_amount_in` token base units for at least
/// `min_quote_amount_out` lamports.
pub fn sell_ix(k: &SwapKeys, user: &Pubkey, base_amount_in: u64, min_quote_amount_out: u64) -> Instruction {
    let mut data = Vec::with_capacity(8 + 8 + 8);
    data.extend_from_slice(&DISC_SELL);
    data.extend_from_slice(&base_amount_in.to_le_bytes());
    data.extend_from_slice(&min_quote_amount_out.to_le_bytes());
    let mut accounts = base_accounts(k, user);
    accounts.extend(remaining_accounts(k, user, true));
    Instruction { program_id: PUMP_AMM_PROGRAM, accounts, data }
}

#[cfg(test)]
mod orientation_tests {
    use super::*;

    #[test]
    fn quote_mint_offset_follows_base() {
        // Both are pubkeys laid out back to back; a wrong offset here would
        // silently match nothing and report "not a pump coin".
        assert_eq!(POOL_QUOTE_MINT_OFFSET, POOL_BASE_MINT_OFFSET + 32);
    }

    #[test]
    fn a_sol_quoted_pool_is_the_normal_orientation() {
        // Guards the meaning of is_sol_quoted(): SOL must be the QUOTE side.
        // Inverted pools (SOL as base) mirror trade direction and are refused.
        assert_ne!(POOL_BASE_MINT_OFFSET, POOL_QUOTE_MINT_OFFSET);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> SwapKeys {
        SwapKeys {
            pool: Pubkey::new_from_array([1u8; 32]),
            base_mint: Pubkey::new_from_array([2u8; 32]),
            quote_mint: NATIVE_MINT,
            base_token_program: super::super::TOKEN_PROGRAM,
            quote_token_program: super::super::TOKEN_PROGRAM,
            pool_base_ta: Pubkey::new_from_array([3u8; 32]),
            pool_quote_ta: Pubkey::new_from_array([4u8; 32]),
            coin_creator: Pubkey::new_from_array([5u8; 32]),
            is_cashback_coin: true,
            buyback_recipient: Some(Pubkey::new_from_array([7u8; 32])),
            fee_recipient: Pubkey::new_from_array([6u8; 32]),
        }
    }

    #[test]
    fn account_counts_match_the_idl() {
        // The test keys are a cashback coin WITH a creator AND a buyback
        // recipient, so every remaining-account group rides: 23 named +
        // cashback ATA + pool_v2 + buyback pair on the buy; 21 named +
        // [ATA, accumulator] + pool_v2 + buyback pair on the sell.
        let user = Pubkey::new_from_array([9u8; 32]);
        let k = keys();
        let buy = buy_ix(&k, &user, 1, 1);
        assert_eq!(buy.accounts.len(), 27, "AMM buy: 23 named + 4 remaining");
        assert_eq!(buy.accounts[23].pubkey, uva_quote_ata(&k, &user));
        assert!(buy.accounts[23].is_writable, "cashback accrues INTO the ATA");
        assert_eq!(buy.accounts[24].pubkey, pool_v2_pda(&k.base_mint));
        assert!(!buy.accounts[24].is_writable, "pool_v2 is verified, not written");
        assert_eq!(buy.accounts[25].pubkey, k.buyback_recipient.unwrap());
        assert_eq!(
            buy.accounts[26].pubkey,
            ata(&k.buyback_recipient.unwrap(), &k.quote_mint, &k.quote_token_program)
        );
        assert!(buy.accounts[26].is_writable);

        let sell = sell_ix(&k, &user, 1, 1);
        assert_eq!(sell.accounts.len(), 26, "AMM sell: 21 named + 5 remaining");
        assert_eq!(sell.accounts[21].pubkey, uva_quote_ata(&k, &user));
        assert_eq!(sell.accounts[22].pubkey, user_volume_accumulator_pda(&user));
        assert_eq!(sell.accounts[23].pubkey, pool_v2_pda(&k.base_mint));
        assert_eq!(sell.accounts[24].pubkey, k.buyback_recipient.unwrap());

        // A plain pool — no cashback, no creator, no buyback readable — sends
        // exactly the named accounts, nothing invented.
        let plain = SwapKeys {
            is_cashback_coin: false,
            buyback_recipient: None,
            coin_creator: Pubkey::default(),
            ..k
        };
        assert_eq!(buy_ix(&plain, &user, 1, 1).accounts.len(), 23);
        assert_eq!(sell_ix(&plain, &user, 1, 1).accounts.len(), 21);
    }

    #[test]
    fn instruction_data_layout() {
        let user = Pubkey::new_from_array([9u8; 32]);
        let b = buy_ix(&keys(), &user, 500_000_000, 42);
        assert_eq!(&b.data[..8], &DISC_BUY_EXACT_QUOTE_IN);
        assert_eq!(&b.data[8..16], &500_000_000u64.to_le_bytes());
        assert_eq!(&b.data[16..24], &42u64.to_le_bytes());
        assert_eq!(b.data.len(), 25, "8 disc + 8 + 8 + track_volume byte");

        let s = sell_ix(&keys(), &user, 1_000, 7);
        assert_eq!(&s.data[..8], &DISC_SELL);
        assert_eq!(s.data.len(), 24, "sell has no track_volume byte");
    }

    /// The user must be the sole signer on both sides.
    #[test]
    fn only_the_user_signs() {
        let user = Pubkey::new_from_array([9u8; 32]);
        for ix in [buy_ix(&keys(), &user, 1, 1), sell_ix(&keys(), &user, 1, 1)] {
            let signers: Vec<_> = ix.accounts.iter().filter(|a| a.is_signer).map(|a| a.pubkey).collect();
            assert_eq!(signers, vec![user]);
        }
    }

    /// The AMM's creator vault uses an UNDERSCORE seed; the bonding curve uses a
    /// HYPHEN. Conflating them silently derives the wrong account.
    #[test]
    fn creator_vault_seed_differs_from_the_bonding_curve() {
        let creator = Pubkey::new_from_array([5u8; 32]);
        assert_ne!(
            coin_creator_vault_authority(&creator),
            super::super::creator_vault_pda(&creator),
            "AMM 'creator_vault' must not equal bonding-curve 'creator-vault'"
        );
    }

    #[test]
    fn constant_product_quotes_behave() {
        // 1000 tokens / 10 SOL pool, 1% fee.
        let (b, q, fee) = (1000.0, 10.0, 0.01);
        let out = tokens_out(b, q, 1.0, fee);
        assert!(out > 0.0 && out < 1000.0);
        // Ten times the SOL buys less than ten times the tokens (slippage).
        assert!(tokens_out(b, q, 10.0, fee) < out * 10.0);
        // Round trip loses to fees.
        let back = sol_out(b - out, q + 1.0, out, fee);
        assert!(back < 1.0, "round trip must not be profitable ({back})");
        assert_eq!(tokens_out(b, q, 0.0, fee), 0.0);
        assert_eq!(sol_out(b, q, -5.0, fee), 0.0);
    }
}


// ---- liquidity -----------------------------------------------------------
//
// Only the AMM has LP. A coin still on its bonding curve has no position to take
// — the curve IS the liquidity — so these apply solely to graduated coins.

/// The 15 accounts shared by `deposit` and `withdraw`, in IDL order.
///
/// Note this list differs from the swap list: the LP mint and the user's LP
/// token account appear, the fee accounts don't, and `user` is a NON-writable
/// signer here (it is writable on swaps). Transcribed from the IDL — don't tidy.
fn lp_accounts(k: &SwapKeys, user: &Pubkey, lp_mint: &Pubkey) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new(k.pool, false),                                            //  1 pool          W
        AccountMeta::new_readonly(global_config_pda(), false),                      //  2 global_config
        AccountMeta::new_readonly(*user, true),                                     //  3 user          signer (ro)
        AccountMeta::new_readonly(k.base_mint, false),                              //  4 base_mint
        AccountMeta::new_readonly(k.quote_mint, false),                             //  5 quote_mint
        AccountMeta::new(*lp_mint, false),                                          //  6 lp_mint       W
        AccountMeta::new(ata(user, &k.base_mint, &k.base_token_program), false),    //  7 user_base_ta  W
        AccountMeta::new(ata(user, &k.quote_mint, &k.quote_token_program), false),  //  8 user_quote_ta W
        AccountMeta::new(ata(user, lp_mint, &super::TOKEN_PROGRAM), false),         //  9 user_lp_ta    W
        AccountMeta::new(k.pool_base_ta, false),                                    // 10 pool_base_ta  W
        AccountMeta::new(k.pool_quote_ta, false),                                   // 11 pool_quote_ta W
        AccountMeta::new_readonly(super::TOKEN_PROGRAM, false),                     // 12 token_program
        AccountMeta::new_readonly(super::TOKEN_2022_PROGRAM, false),                // 13 token_2022
        AccountMeta::new_readonly(event_authority_pda(), false),                    // 14 event_authority
        AccountMeta::new_readonly(PUMP_AMM_PROGRAM, false),                         // 15 program
    ]
}

/// `deposit`: mint `lp_token_amount_out` LP tokens, spending at most
/// `max_base_amount_in` tokens and `max_quote_amount_in` lamports.
///
/// The maxima ARE the slippage protection — the pool ratio moves between quote
/// and execution, and the program reverts rather than taking more than allowed.
pub fn deposit_ix(
    k: &SwapKeys,
    user: &Pubkey,
    lp_mint: &Pubkey,
    lp_token_amount_out: u64,
    max_base_amount_in: u64,
    max_quote_amount_in: u64,
) -> Instruction {
    let mut data = Vec::with_capacity(8 + 24);
    data.extend_from_slice(&DISC_DEPOSIT);
    data.extend_from_slice(&lp_token_amount_out.to_le_bytes());
    data.extend_from_slice(&max_base_amount_in.to_le_bytes());
    data.extend_from_slice(&max_quote_amount_in.to_le_bytes());
    Instruction { program_id: PUMP_AMM_PROGRAM, accounts: lp_accounts(k, user, lp_mint), data }
}

/// `withdraw`: burn `lp_token_amount_in` LP tokens for at least
/// `min_base_amount_out` tokens and `min_quote_amount_out` lamports.
pub fn withdraw_ix(
    k: &SwapKeys,
    user: &Pubkey,
    lp_mint: &Pubkey,
    lp_token_amount_in: u64,
    min_base_amount_out: u64,
    min_quote_amount_out: u64,
) -> Instruction {
    let mut data = Vec::with_capacity(8 + 24);
    data.extend_from_slice(&DISC_WITHDRAW);
    data.extend_from_slice(&lp_token_amount_in.to_le_bytes());
    data.extend_from_slice(&min_base_amount_out.to_le_bytes());
    data.extend_from_slice(&min_quote_amount_out.to_le_bytes());
    Instruction { program_id: PUMP_AMM_PROGRAM, accounts: lp_accounts(k, user, lp_mint), data }
}

/// LP tokens minted for depositing `quote_in` SOL at the current ratio, and the
/// matching base-token amount required.
///
/// A deposit must be BALANCED: supplying SOL alone would just be a swap. This
/// returns `(lp_out, base_needed)` so a caller can check it holds enough of both.
pub fn deposit_quote(
    base_res: f64,
    quote_res: f64,
    lp_supply: f64,
    quote_in: f64,
) -> (f64, f64) {
    if quote_res <= 0.0 || base_res <= 0.0 || lp_supply <= 0.0 || quote_in <= 0.0 {
        return (0.0, 0.0);
    }
    let share = quote_in / quote_res;
    (lp_supply * share, base_res * share)
}

/// Tokens and SOL returned for burning `lp_in` LP tokens.
pub fn withdraw_quote(base_res: f64, quote_res: f64, lp_supply: f64, lp_in: f64) -> (f64, f64) {
    if lp_supply <= 0.0 || lp_in <= 0.0 {
        return (0.0, 0.0);
    }
    let share = (lp_in / lp_supply).min(1.0);
    (base_res * share, quote_res * share)
}

#[cfg(test)]
mod lp_tests {
    use super::*;

    fn k() -> SwapKeys {
        SwapKeys {
            pool: Pubkey::new_from_array([1u8; 32]),
            base_mint: Pubkey::new_from_array([2u8; 32]),
            quote_mint: NATIVE_MINT,
            base_token_program: super::super::TOKEN_PROGRAM,
            quote_token_program: super::super::TOKEN_PROGRAM,
            pool_base_ta: Pubkey::new_from_array([3u8; 32]),
            pool_quote_ta: Pubkey::new_from_array([4u8; 32]),
            coin_creator: Pubkey::new_from_array([5u8; 32]),
            is_cashback_coin: true,
            buyback_recipient: Some(Pubkey::new_from_array([7u8; 32])),
            fee_recipient: Pubkey::new_from_array([6u8; 32]),
        }
    }

    #[test]
    fn lp_account_counts_match_the_idl() {
        let (user, lp) = (Pubkey::new_from_array([9u8; 32]), Pubkey::new_from_array([8u8; 32]));
        assert_eq!(deposit_ix(&k(), &user, &lp, 1, 1, 1).accounts.len(), 15);
        assert_eq!(withdraw_ix(&k(), &user, &lp, 1, 1, 1).accounts.len(), 15);
    }

    /// On LP instructions `user` is a NON-writable signer, unlike on swaps.
    #[test]
    fn user_is_a_readonly_signer_on_lp() {
        let (user, lp) = (Pubkey::new_from_array([9u8; 32]), Pubkey::new_from_array([8u8; 32]));
        let ix = deposit_ix(&k(), &user, &lp, 1, 1, 1);
        let u = &ix.accounts[2];
        assert_eq!(u.pubkey, user);
        assert!(u.is_signer, "user must sign");
        assert!(!u.is_writable, "but is read-only on LP ops, per the IDL");
        // ...whereas on a swap the user IS writable.
        assert!(buy_ix(&k(), &user, 1, 1).accounts[1].is_writable);
    }

    #[test]
    fn lp_data_layout() {
        let (user, lp) = (Pubkey::new_from_array([9u8; 32]), Pubkey::new_from_array([8u8; 32]));
        let d = deposit_ix(&k(), &user, &lp, 10, 20, 30);
        assert_eq!(&d.data[..8], &DISC_DEPOSIT);
        assert_eq!(&d.data[8..16], &10u64.to_le_bytes());
        assert_eq!(&d.data[16..24], &20u64.to_le_bytes());
        assert_eq!(&d.data[24..32], &30u64.to_le_bytes());
        assert_eq!(d.data.len(), 32);

        let w = withdraw_ix(&k(), &user, &lp, 5, 6, 7);
        assert_eq!(&w.data[..8], &DISC_WITHDRAW);
        assert_eq!(w.data.len(), 32);
    }

    /// A deposit is proportional: half the pool's SOL needs half its tokens.
    #[test]
    fn deposit_is_balanced_and_proportional() {
        let (base, quote, lp) = (1_000_000.0, 100.0, 5_000.0);
        let (lp_out, base_needed) = deposit_quote(base, quote, lp, 10.0);
        assert!((lp_out - 500.0).abs() < 1e-9, "10% of SOL -> 10% of LP");
        assert!((base_needed - 100_000.0).abs() < 1e-6, "and 10% of the tokens");
        // Degenerate inputs return nothing rather than NaN/inf.
        assert_eq!(deposit_quote(0.0, quote, lp, 10.0), (0.0, 0.0));
        assert_eq!(deposit_quote(base, quote, lp, 0.0), (0.0, 0.0));
    }

    #[test]
    fn withdraw_returns_the_pro_rata_share() {
        let (base, quote, lp) = (1_000_000.0, 100.0, 5_000.0);
        let (b, q) = withdraw_quote(base, quote, lp, 2_500.0);
        assert!((b - 500_000.0).abs() < 1e-6);
        assert!((q - 50.0).abs() < 1e-9);
        // Burning more than exists can't return more than the pool holds.
        let (b2, q2) = withdraw_quote(base, quote, lp, lp * 10.0);
        assert!(b2 <= base && q2 <= quote);
    }
}
