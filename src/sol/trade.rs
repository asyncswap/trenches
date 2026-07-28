// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs
//! pump.fun bonding-curve trade instructions, hand-encoded.
//!
//! We build the instructions ourselves rather than routing through an aggregator
//! so nothing external sits in the hot path. Discriminators and — critically —
//! **account order** come from the official IDL
//! (`../pump-public-docs/idl/pump.json`), cross-checked against
//! `../carbon/decoders/pumpfun-decoder`.
//!
//! The legacy `buy`/`sell` variants are used (16 and 14 accounts) rather than
//! `buy_v2`/`sell_v2` (27 and 26): far less surface to get wrong, same curve.
//!
//! ⚠️ Account order is NOT the same between buy and sell — `creator_vault` comes
//! after `token_program` in buy, but *before* it in sell. The orders below are
//! transcribed from the IDL; don't "tidy" them.

use borsh::BorshDeserialize;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;

use super::{
    ata, bonding_curve_pda, creator_vault_pda, event_authority_pda, fee_config_pda, global_pda,
    global_volume_accumulator_pda, sharing_config_pda, user_volume_accumulator_pda, ATA_PROGRAM,
    NATIVE_MINT, PUMP_FEES_PROGRAM, PUMP_PROGRAM, SYSTEM_PROGRAM, TOKEN_PROGRAM,
};

/// Anchor discriminators (8-byte instruction selectors) from the IDL.
const DISC_BUY: [u8; 8] = [102, 6, 61, 18, 1, 218, 235, 234];
/// `buy_exact_sol_in` — takes SOL in + a token floor, rather than `buy`'s
/// tokens-out + SOL cap. This is what live pump.fun traffic uses (verified by
/// decoding recent mainnet transactions), and it matches how this bot sizes
/// trades ("spend N SOL"), so it's the default buy path.
const DISC_BUY_EXACT_SOL_IN: [u8; 8] = [56, 252, 116, 8, 158, 223, 205, 95];
const DISC_SELL: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];

/// Anchor discriminator for the `Global` config account.
const GLOBAL_DISC: [u8; 8] = [167, 232, 232, 177, 200, 108, 114, 127];

/// The subset of `Global` we need: which address collects protocol fees. The
/// account has many more fields, but borsh reads them in order, so we stop
/// after the ones we use.
#[derive(Debug, Clone, BorshDeserialize)]
pub struct GlobalHead {
    pub initialized: bool,
    pub authority: Pubkey,
    /// Primary protocol fee recipient — passed as account #2 of buy/sell.
    pub fee_recipient: Pubkey,
    pub initial_virtual_token_reserves: u64,
    pub initial_virtual_sol_reserves: u64,
    pub initial_real_token_reserves: u64,
    pub token_total_supply: u64,
    pub fee_basis_points: u64,
}

impl GlobalHead {
    pub fn decode(data: &[u8]) -> eyre::Result<GlobalHead> {
        if data.len() < 8 || data[..8] != GLOBAL_DISC {
            eyre::bail!("not a pump Global account (discriminator mismatch)");
        }
        let mut rest = &data[8..];
        GlobalHead::deserialize(&mut rest).map_err(|e| eyre::eyre!("global decode: {e}"))
    }

    /// The buyback fee recipient the v2 trade instructions require.
    ///
    /// pump routes part of the protocol fee to a buyback, and a trade that
    /// names no recipient is rejected with `BuybackFeeRecipientMissing`
    /// (6062). The eight slots are interchangeable — live traffic uses
    /// different ones — so any non-default entry is valid.
    ///
    /// Read by offset rather than by extending [`GlobalHead`]: this field sits
    /// behind ~700 bytes of unrelated config, and a strict decode of all of it
    /// would turn any future field pump appends into a total failure to load
    /// ANY coin. Here a layout change costs the buyback recipient only.
    pub fn buyback_recipient(data: &[u8]) -> Option<Pubkey> {
        // Offsets of `Global`, in declaration order, from the IDL.
        const OFF: usize = 8            // anchor discriminator
            + 1 + 32 + 32               // initialized, authority, fee_recipient
            + 8 * 5                     // initial reserves, total supply, fee bps
            + 32 + 1 + 8 + 8            // withdraw_authority .. creator_fee_bps
            + 32 * 7                    // fee_recipients
            + 32 + 32 + 1               // set/admin creator authority, create_v2_enabled
            + 32 + 32 + 1               // whitelist_pda, reserved_fee_recipient, mayhem
            + 32 * 7                    // reserved_fee_recipients
            + 1; // is_cashback_enabled
        if data.len() < 8 || data[..8] != GLOBAL_DISC {
            return None;
        }
        data.get(OFF..OFF + 32 * 8)?
            .chunks_exact(32)
            .filter_map(|c| Pubkey::try_from(c).ok())
            .find(|p| *p != Pubkey::default())
    }

    /// The creator fee, in basis points, charged on curve trades on top of
    /// [`GlobalHead::fee_frac`]. Read by offset for the same reason as the
    /// recipients above.
    pub fn creator_fee_frac(data: &[u8]) -> Option<f64> {
        const OFF: usize = 8 + 1 + 32 + 32 + 8 * 5 + 32 + 1 + 8;
        if data.len() < 8 || data[..8] != GLOBAL_DISC {
            return None;
        }
        let bps = u64::from_le_bytes(data.get(OFF..OFF + 8)?.try_into().ok()?);
        // A "creator fee" above 10% would mean we are reading the wrong bytes.
        (bps <= 1_000).then(|| bps as f64 / 10_000.0)
    }

    /// The fee recipient for a "mayhem mode" coin.
    ///
    /// Mayhem coins are billed to a SEPARATE set of recipients
    /// (`reserved_fee_recipient`), and passing the normal one is rejected. Read
    /// by offset for the same reason as [`GlobalHead::buyback_recipient`].
    pub fn reserved_fee_recipient(data: &[u8]) -> Option<Pubkey> {
        const OFF: usize = 8 + 1 + 32 + 32 + 8 * 5 + 32 + 1 + 8 + 8 + 32 * 7 + 32 + 32 + 1 + 32;
        if data.len() < 8 || data[..8] != GLOBAL_DISC {
            return None;
        }
        let p = Pubkey::try_from(data.get(OFF..OFF + 32)?).ok()?;
        (p != Pubkey::default()).then_some(p)
    }

    /// Protocol fee as a fraction (e.g. 0.01 for 100 bps).
    pub fn fee_frac(&self) -> f64 {
        self.fee_basis_points as f64 / 10_000.0
    }
}

/// Everything needed to address a coin's curve, resolved once per pool.
#[derive(Debug, Clone, Copy)]
pub struct CurveKeys {
    pub mint: Pubkey,
    /// What the coin trades AGAINST. Almost always wrapped SOL, but pump now
    /// whitelists other quote mints (USDC among them), and every
    /// `associated_quote_*` account in a v2 trade is seed-derived from it —
    /// so assuming SOL on a USDC-quoted coin fails the whole instruction with
    /// `ConstraintSeeds`.
    pub quote_mint: Pubkey,
    /// The mint's owning program — SPL Token or Token-2022. Must be read from
    /// the mint account, never assumed.
    pub token_program: Pubkey,
    pub bonding_curve: Pubkey,
    pub associated_bonding_curve: Pubkey,
    /// Curve creator, from the decoded `BondingCurve` — seeds `creator_vault`.
    pub creator: Pubkey,
}

impl CurveKeys {
    /// `quote_mint` comes from the coin's `BondingCurve`. It is
    /// `Pubkey::default()` for every SOL-paired coin, which — like wrapped SOL
    /// itself — means "legacy quote" and is passed to the program as WSOL.
    pub fn new(mint: Pubkey, token_program: Pubkey, creator: Pubkey, quote_mint: Pubkey) -> CurveKeys {
        let bonding_curve = bonding_curve_pda(&mint);
        let quote_mint = if quote_mint == Pubkey::default() { NATIVE_MINT } else { quote_mint };
        CurveKeys {
            mint,
            quote_mint,
            token_program,
            bonding_curve,
            associated_bonding_curve: ata(&bonding_curve, &mint, &token_program),
            creator,
        }
    }
}

/// Create the user's token account if absent. Idempotent (ATA instruction #1),
/// so it's safe to prepend to every buy without checking first — a wasted
/// check costs an RPC round trip in the hot path; this costs nothing when the
/// account already exists.
pub fn create_ata_idempotent(payer: &Pubkey, owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Instruction {
    Instruction {
        program_id: ATA_PROGRAM,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(ata(owner, mint, token_program), false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM, false),
            AccountMeta::new_readonly(*token_program, false),
        ],
        data: vec![1], // CreateIdempotent
    }
}

/// The 16 accounts shared by `buy` and `buy_exact_sol_in` — identical list and
/// order; only the instruction data differs.
///
/// Writability is copied exactly from the IDL. It is NOT cosmetic:
/// `global_volume_accumulator` is read-only and shared by every pump trader, so
/// marking it writable would take a global write lock and serialise us behind
/// every other pump trade — a direct hit to landing rate for a latency-sensitive
/// bot (and Anchor may reject it outright).
fn buy_accounts(keys: &CurveKeys, user: &Pubkey, fee_recipient: &Pubkey) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new_readonly(global_pda(), false),                       //  1 global            ro
        AccountMeta::new(*fee_recipient, false),                              //  2 fee_recipient     W
        AccountMeta::new_readonly(keys.mint, false),                          //  3 mint              ro
        AccountMeta::new(keys.bonding_curve, false),                          //  4 bonding_curve     W
        AccountMeta::new(keys.associated_bonding_curve, false),               //  5 assoc_bonding_crv W
        AccountMeta::new(ata(user, &keys.mint, &keys.token_program), false),  //  6 associated_user   W
        AccountMeta::new(*user, true),                                        //  7 user              W signer
        AccountMeta::new_readonly(SYSTEM_PROGRAM, false),                     //  8 system_program    ro
        AccountMeta::new_readonly(keys.token_program, false),                 //  9 token_program     ro
        AccountMeta::new(creator_vault_pda(&keys.creator), false),            // 10 creator_vault     W
        AccountMeta::new_readonly(event_authority_pda(), false),              // 11 event_authority   ro
        AccountMeta::new_readonly(PUMP_PROGRAM, false),                       // 12 program           ro
        AccountMeta::new_readonly(global_volume_accumulator_pda(), false),    // 13 global_vol_accum  ro  <-- NOT writable
        AccountMeta::new(user_volume_accumulator_pda(user), false),           // 14 user_vol_accum    W
        AccountMeta::new_readonly(fee_config_pda(), false),                   // 15 fee_config        ro
        AccountMeta::new_readonly(PUMP_FEES_PROGRAM, false),                  // 16 fee_program       ro
    ]
}

/// `buy_exact_sol_in` — spend exactly `spendable_sol_in` lamports, requiring at
/// least `min_tokens_out` base units back. **This is the preferred buy path.**
///
/// It fits how the bot sizes trades (a SOL amount), and avoids `buy`'s awkward
/// two-step of quoting tokens-out and then capping SOL. `min_tokens_out` is the
/// slippage protection — the program reverts below it, so never pass 0.
pub fn buy_exact_sol_in_ix(
    keys: &CurveKeys,
    user: &Pubkey,
    fee_recipient: &Pubkey,
    spendable_sol_in: u64,
    min_tokens_out: u64,
) -> Instruction {
    let mut data = Vec::with_capacity(8 + 8 + 8 + 1);
    data.extend_from_slice(&DISC_BUY_EXACT_SOL_IN);
    data.extend_from_slice(&spendable_sol_in.to_le_bytes());
    data.extend_from_slice(&min_tokens_out.to_le_bytes());
    data.push(1); // track_volume: OptionBool — one trailing byte

    Instruction { program_id: PUMP_PROGRAM, accounts: buy_accounts(keys, user, fee_recipient), data }
}

/// `buy` (legacy): receive exactly `amount` token base units, paying at most
/// `max_sol_cost` lamports. Kept for completeness; prefer
/// [`buy_exact_sol_in_ix`], which is what live traffic uses and what this bot's
/// sizing model wants.
pub fn buy_ix(keys: &CurveKeys, user: &Pubkey, fee_recipient: &Pubkey, amount: u64, max_sol_cost: u64) -> Instruction {
    let mut data = Vec::with_capacity(8 + 8 + 8 + 1);
    data.extend_from_slice(&DISC_BUY);
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&max_sol_cost.to_le_bytes());
    data.push(1); // track_volume: OptionBool

    Instruction { program_id: PUMP_PROGRAM, accounts: buy_accounts(keys, user, fee_recipient), data }
}

/// `sell`: sell `amount` token base units for at least `min_sol_output` lamports.
/// `min_sol_output` is the slippage floor — the program reverts below it.
///
/// Note the account order differs from `buy`: `creator_vault` precedes
/// `token_program` here, and there are no volume-accumulator accounts.
pub fn sell_ix(keys: &CurveKeys, user: &Pubkey, fee_recipient: &Pubkey, amount: u64, min_sol_output: u64) -> Instruction {
    let mut data = Vec::with_capacity(8 + 8 + 8);
    data.extend_from_slice(&DISC_SELL);
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&min_sol_output.to_le_bytes());

    Instruction {
        program_id: PUMP_PROGRAM,
        accounts: vec![
            AccountMeta::new_readonly(global_pda(), false),                       //  1 global
            AccountMeta::new(*fee_recipient, false),                              //  2 fee_recipient
            AccountMeta::new_readonly(keys.mint, false),                          //  3 mint
            AccountMeta::new(keys.bonding_curve, false),                          //  4 bonding_curve
            AccountMeta::new(keys.associated_bonding_curve, false),               //  5 assoc_bonding_curve
            AccountMeta::new(ata(user, &keys.mint, &keys.token_program), false),  //  6 associated_user
            AccountMeta::new(*user, true),                                        //  7 user (signer)
            AccountMeta::new_readonly(SYSTEM_PROGRAM, false),                     //  8 system_program
            AccountMeta::new(creator_vault_pda(&keys.creator), false),            //  9 creator_vault
            AccountMeta::new_readonly(keys.token_program, false),                 // 10 token_program
            AccountMeta::new_readonly(event_authority_pda(), false),              // 11 event_authority
            AccountMeta::new_readonly(PUMP_PROGRAM, false),                       // 12 program
            AccountMeta::new_readonly(fee_config_pda(), false),                   // 13 fee_config
            AccountMeta::new_readonly(PUMP_FEES_PROGRAM, false),                  // 14 fee_program
        ],
        data,
    }
}

/// Apply a slippage tolerance to a quoted amount.
/// - buys: the cap must sit *above* the quote (`up = true`)
/// - sells: the floor must sit *below* it (`up = false`)
pub fn with_slippage(quote: u64, pct: f64, up: bool) -> u64 {
    let f = (pct.max(0.0)) / 100.0;
    let v = quote as f64;
    if up { (v * (1.0 + f)) as u64 } else { (v * (1.0 - f)) as u64 }
}

/// Discriminators for the v2 trade instructions.
const DISC_BUY_EXACT_QUOTE_IN_V2: [u8; 8] = [194, 171, 28, 70, 104, 77, 91, 47];
const DISC_SELL_V2: [u8; 8] = [93, 246, 130, 60, 231, 233, 64, 178];

/// The account list shared by `buy_exact_quote_in_v2` and `sell_v2`.
///
/// v2 replaced the legacy pair's implicit "remaining accounts" convention with
/// a fully declared list, which is why we use it: every account below is
/// derivable, so there is nothing left to guess. The buy form additionally
/// carries `global_volume_accumulator` at index 19.
///
/// SOL-paired coins still trade in native SOL — the `associated_quote_*`
/// accounts are only seed-constrained to the quote mint and need not exist, so
/// passing them costs nothing and no SOL wrapping is involved.
fn v2_accounts(
    keys: &CurveKeys,
    user: &Pubkey,
    fee_recipient: &Pubkey,
    buyback: &Pubkey,
    buy: bool,
) -> Vec<AccountMeta> {
    let quote = keys.quote_mint;
    // Both WSOL and the whitelisted quote mints are plain SPL Token, which is
    // what pump's own SDK assumes for the quote side.
    let qtp = TOKEN_PROGRAM;
    let curve = keys.bonding_curve;
    let vault = creator_vault_pda(&keys.creator);
    let uva = user_volume_accumulator_pda(user);
    let mut accounts = vec![
        AccountMeta::new_readonly(global_pda(), false),                        //  0 global
        AccountMeta::new_readonly(keys.mint, false),                           //  1 base_mint
        AccountMeta::new_readonly(quote, false),                               //  2 quote_mint
        AccountMeta::new_readonly(keys.token_program, false),                  //  3 base_token_program
        AccountMeta::new_readonly(qtp, false),                                 //  4 quote_token_program
        AccountMeta::new_readonly(ATA_PROGRAM, false),                         //  5 associated_token_program
        AccountMeta::new(*fee_recipient, false),                               //  6 fee_recipient
        AccountMeta::new(ata(fee_recipient, &quote, &qtp), false),             //  7 assoc_quote_fee_recipient
        AccountMeta::new(*buyback, false),                                     //  8 buyback_fee_recipient
        AccountMeta::new(ata(buyback, &quote, &qtp), false),                   //  9 assoc_quote_buyback
        AccountMeta::new(curve, false),                                        // 10 bonding_curve
        AccountMeta::new(ata(&curve, &keys.mint, &keys.token_program), false), // 11 assoc_base_curve
        AccountMeta::new(ata(&curve, &quote, &qtp), false),                    // 12 assoc_quote_curve
        AccountMeta::new(*user, true),                                         // 13 user (signer)
        AccountMeta::new(ata(user, &keys.mint, &keys.token_program), false),   // 14 assoc_base_user
        AccountMeta::new(ata(user, &quote, &qtp), false),                      // 15 assoc_quote_user
        AccountMeta::new(vault, false),                                        // 16 creator_vault
        AccountMeta::new(ata(&vault, &quote, &qtp), false),                    // 17 assoc_creator_vault
        AccountMeta::new_readonly(sharing_config_pda(&keys.mint), false),      // 18 sharing_config
    ];
    if buy {
        // Read-only on purpose: it is shared by every pump trader, so a write
        // lock would serialise us behind all other pump traffic.
        accounts.push(AccountMeta::new_readonly(global_volume_accumulator_pda(), false));
    }
    accounts.extend([
        AccountMeta::new(uva, false),                        // user_volume_accumulator
        AccountMeta::new(ata(&uva, &quote, &qtp), false),    // assoc_user_volume_accumulator
        AccountMeta::new_readonly(fee_config_pda(), false),  // fee_config
        AccountMeta::new_readonly(PUMP_FEES_PROGRAM, false), // fee_program
        AccountMeta::new_readonly(SYSTEM_PROGRAM, false),    // system_program
        AccountMeta::new_readonly(event_authority_pda(), false), // event_authority
        AccountMeta::new_readonly(PUMP_PROGRAM, false),      // program
    ]);
    accounts
}

/// `buy_exact_quote_in_v2` — spend exactly `spendable_quote_in` lamports,
/// requiring at least `min_tokens_out` base units back. The v2 replacement for
/// [`buy_exact_sol_in_ix`], which pump now rejects without a buyback recipient.
pub fn buy_exact_quote_in_v2_ix(
    keys: &CurveKeys,
    user: &Pubkey,
    fee_recipient: &Pubkey,
    buyback: &Pubkey,
    spendable_quote_in: u64,
    min_tokens_out: u64,
) -> Instruction {
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&DISC_BUY_EXACT_QUOTE_IN_V2);
    data.extend_from_slice(&spendable_quote_in.to_le_bytes());
    data.extend_from_slice(&min_tokens_out.to_le_bytes());
    Instruction {
        program_id: PUMP_PROGRAM,
        accounts: v2_accounts(keys, user, fee_recipient, buyback, true),
        data,
    }
}

/// `sell_v2` — sell `amount` base units for at least `min_sol_output` lamports.
pub fn sell_v2_ix(
    keys: &CurveKeys,
    user: &Pubkey,
    fee_recipient: &Pubkey,
    buyback: &Pubkey,
    amount: u64,
    min_sol_output: u64,
) -> Instruction {
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&DISC_SELL_V2);
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&min_sol_output.to_le_bytes());
    Instruction {
        program_id: PUMP_PROGRAM,
        accounts: v2_accounts(keys, user, fee_recipient, buyback, false),
        data,
    }
}

/// System-program transfer: `from` -> `to`, in lamports.
///
/// Wrapping SOL is a plain transfer INTO the WSOL token account followed by
/// `sync_native` — there is no "wrap" instruction.
pub fn transfer_lamports(from: &Pubkey, to: &Pubkey, lamports: u64) -> Instruction {
    let mut data = Vec::with_capacity(12);
    data.extend_from_slice(&2u32.to_le_bytes()); // SystemInstruction::Transfer
    data.extend_from_slice(&lamports.to_le_bytes());
    Instruction {
        program_id: super::SYSTEM_PROGRAM,
        accounts: vec![AccountMeta::new(*from, true), AccountMeta::new(*to, false)],
        data,
    }
}

/// `SyncNative` — makes a WSOL token account's balance match the lamports that
/// were transferred into it. Without this the token program still sees the OLD
/// amount, and the swap fails with InsufficientFunds even though the lamports
/// are sitting right there.
pub fn sync_native(account: &Pubkey, token_program: &Pubkey) -> Instruction {
    Instruction {
        program_id: *token_program,
        accounts: vec![AccountMeta::new(*account, false)],
        data: vec![17], // TokenInstruction::SyncNative
    }
}

/// `CloseAccount` — closes a token account, sending its lamports (rent AND any
/// wrapped SOL) to `dest`. Closing the WSOL account after a trade is what turns
/// wrapped proceeds back into spendable SOL.
pub fn close_token_account(
    account: &Pubkey,
    dest: &Pubkey,
    owner: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: *token_program,
        accounts: vec![
            AccountMeta::new(*account, false),
            AccountMeta::new(*dest, false),
            AccountMeta::new_readonly(*owner, true),
        ],
        data: vec![9], // TokenInstruction::CloseAccount
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> CurveKeys {
        // Deterministic stand-ins; PDAs are derived, so the values don't matter.
        let mint = Pubkey::new_from_array([7u8; 32]);
        CurveKeys::new(mint, super::super::TOKEN_PROGRAM, Pubkey::new_from_array([9u8; 32]), Pubkey::default())
    }

    #[test]
    fn buy_layout_matches_idl() {
        let user = Pubkey::new_from_array([3u8; 32]);
        let fee = Pubkey::new_from_array([4u8; 32]);
        let ix = buy_ix(&keys(), &user, &fee, 1_000_000, 50_000_000);

        assert_eq!(ix.program_id, PUMP_PROGRAM);
        assert_eq!(ix.accounts.len(), 16, "legacy buy takes exactly 16 accounts");
        assert_eq!(&ix.data[..8], &DISC_BUY);
        // amount, max_sol_cost little-endian, then the OptionBool byte.
        assert_eq!(&ix.data[8..16], &1_000_000u64.to_le_bytes());
        assert_eq!(&ix.data[16..24], &50_000_000u64.to_le_bytes());
        assert_eq!(ix.data.len(), 25, "8 disc + 8 + 8 + 1 trailing OptionBool");

        // The signer must be the user, and only the user.
        let signers: Vec<_> = ix.accounts.iter().filter(|a| a.is_signer).map(|a| a.pubkey).collect();
        assert_eq!(signers, vec![user]);
        // Position-sensitive spot checks against the IDL ordering.
        assert_eq!(ix.accounts[1].pubkey, fee, "#2 must be fee_recipient");
        assert_eq!(ix.accounts[6].pubkey, user, "#7 must be user");
        assert_eq!(ix.accounts[8].pubkey, super::super::TOKEN_PROGRAM, "#9 must be token_program");
        assert_eq!(ix.accounts[11].pubkey, PUMP_PROGRAM, "#12 must be program");
    }

    #[test]
    fn sell_layout_matches_idl_and_differs_from_buy() {
        let user = Pubkey::new_from_array([3u8; 32]);
        let fee = Pubkey::new_from_array([4u8; 32]);
        let k = keys();
        let ix = sell_ix(&k, &user, &fee, 2_000_000, 10_000);

        assert_eq!(ix.accounts.len(), 14, "legacy sell takes exactly 14 accounts");
        assert_eq!(&ix.data[..8], &DISC_SELL);
        assert_eq!(ix.data.len(), 24, "sell has no trailing OptionBool");

        // The ordering trap: creator_vault before token_program on sell.
        assert_eq!(ix.accounts[8].pubkey, creator_vault_pda(&k.creator), "#9 must be creator_vault");
        assert_eq!(ix.accounts[9].pubkey, k.token_program, "#10 must be token_program");

        // ...whereas buy has them the other way round.
        let buy = buy_ix(&k, &user, &fee, 1, 1);
        assert_eq!(buy.accounts[8].pubkey, k.token_program);
        assert_eq!(buy.accounts[9].pubkey, creator_vault_pda(&k.creator));
    }

    /// Writability must match the IDL exactly. `global_volume_accumulator` in
    /// particular is read-only and shared by every pump trader — marking it
    /// writable takes a global write lock and serialises us behind all other
    /// pump traffic. Regression guard for a bug caught by decoding live txs.
    #[test]
    fn account_writability_matches_idl() {
        let user = Pubkey::new_from_array([3u8; 32]);
        let fee = Pubkey::new_from_array([4u8; 32]);
        let k = keys();

        for ix in [
            buy_ix(&k, &user, &fee, 1, 1),
            buy_exact_sol_in_ix(&k, &user, &fee, 1, 1),
        ] {
            // (index, writable) straight from idl/pump.json.
            let expect = [
                (0, false), (1, true),  (2, false), (3, true),
                (4, true),  (5, true),  (6, true),  (7, false),
                (8, false), (9, true),  (10, false), (11, false),
                (12, false), // global_volume_accumulator — read-only
                (13, true), (14, false), (15, false),
            ];
            for (i, w) in expect {
                assert_eq!(ix.accounts[i].is_writable, w, "account #{} writability", i + 1);
            }
        }

        let s = sell_ix(&k, &user, &fee, 1, 1);
        for (i, w) in [(0, false), (1, true), (2, false), (3, true), (4, true), (5, true),
                       (6, true), (7, false), (8, true), (9, false), (10, false), (11, false),
                       (12, false), (13, false)] {
            assert_eq!(s.accounts[i].is_writable, w, "sell account #{} writability", i + 1);
        }
    }

    /// `buy_exact_sol_in` shares `buy`'s account list exactly — only the
    /// discriminator and the meaning of the two u64 args differ.
    #[test]
    fn buy_exact_sol_in_matches_live_traffic_shape() {
        let user = Pubkey::new_from_array([3u8; 32]);
        let fee = Pubkey::new_from_array([4u8; 32]);
        let k = keys();

        let ix = buy_exact_sol_in_ix(&k, &user, &fee, 500_000_000, 12_345);
        assert_eq!(ix.accounts.len(), 16);
        assert_eq!(&ix.data[..8], &DISC_BUY_EXACT_SOL_IN);
        assert_eq!(&ix.data[8..16], &500_000_000u64.to_le_bytes(), "spendable_sol_in");
        assert_eq!(&ix.data[16..24], &12_345u64.to_le_bytes(), "min_tokens_out");
        assert_eq!(ix.data.len(), 25, "8 disc + 8 + 8 + trailing OptionBool");

        // Identical accounts to legacy buy, so one shared builder is correct.
        let legacy = buy_ix(&k, &user, &fee, 1, 1);
        let a: Vec<_> = ix.accounts.iter().map(|m| (m.pubkey, m.is_signer, m.is_writable)).collect();
        let b: Vec<_> = legacy.accounts.iter().map(|m| (m.pubkey, m.is_signer, m.is_writable)).collect();
        assert_eq!(a, b, "buy and buy_exact_sol_in must share the account list");
        assert_ne!(&ix.data[..8], &legacy.data[..8], "but not the discriminator");
    }

    #[test]
    fn slippage_moves_the_right_way() {
        // Buy cap goes up, sell floor goes down — inverting these silently
        // removes all protection, so both directions are asserted.
        assert_eq!(with_slippage(1_000, 10.0, true), 1_100);
        assert_eq!(with_slippage(1_000, 10.0, false), 900);
        assert_eq!(with_slippage(1_000, 0.0, true), 1_000);
    }

    #[test]
    fn ata_creation_is_idempotent_variant() {
        let ix = create_ata_idempotent(
            &Pubkey::new_from_array([3u8; 32]),
            &Pubkey::new_from_array([3u8; 32]),
            &Pubkey::new_from_array([7u8; 32]),
            &super::super::TOKEN_PROGRAM,
        );
        assert_eq!(ix.program_id, ATA_PROGRAM);
        assert_eq!(ix.data, vec![1], "must be CreateIdempotent, not Create");
    }
}

#[cfg(test)]
mod mainnet_verification {
    use super::*;

    /// Real `Global` account data fetched from mainnet-beta
    /// (4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf). Decoding live bytes with
    /// our own struct is the only way to prove the field offsets are right —
    /// unit tests with synthetic data can't catch a layout mistake.
    const LIVE_GLOBAL_B64: &str = "p+joschscn8B07uMqzQc4FKEV/LDgX0yeEQZY9zVX+1YuiTJmd2sAqpKwvjQ3Vy8l+MonBl8tQYqVPPZVrnOblEV+WVnqlyz5gAQ2EfjzwMAAKwj/AYAAAAAeMX7UdECAACAxqR+jQMAXwAAAAAAAAAf6nQ58860xO9Lucx77kChpiYXG2hBX+3tQLeolW+E5wHB4eQAAAAAAAUAAAAAAAAAYIzMHfzpYbQ7d5wZFQWm4tO/RdWk20YYrXbILWF1RTVjg3MADqIssmTTSv9koEte+r+7dN3NBImXsZgVR9fREIOEdCkuZ1qUtDbssKmYiUIyioPdxiM4ApYSZ8XNYRfLjRgaDISfqTem80re0wge+VcAqssMm7PZCaS5FHUnpOutEeak/ClEpPqCUb74FUJuG/soxrZkZndgfGrZ9WamRteqj7Bg2CkbTE1HXa/3Yslr3A2s6zbAEurRLtOpSEFh4ATIfOuY+lzkf4A4Bv0seUXSlSSVmuwA3tl4FPOPeEYf6nQ58860xO9Lucx77kChpiYXG2hBX+3tQLeolW+E5wchXZlAeTaU4RYGbORZuBj9+bugx7QbeD+joSDKQZUyAaKLX9JqtHmmqcxsv2sLI+thiFo3HgEgrKkTvu89E4p46JMUH7GOnxV02BDheOGeMGBOMXWqLkoy38hgByfRBwkBNYRTYlYJT5EoGRJ++k5Ea0MzcheT0Th2+arb89x9C19udQGCIPlCZ3ADI3tNa0U3WbSlxpC1nDXZuxh6CQy9KjOYep67E2eZq1mSWxPl3Iswgd8AXbQnwUePpG/4w0egdOlUPz43otBGInrdy06cd0xEJYxD7fJKqKrh8AIUZlvaTDjNbbdDj1m0CLuew7TKnorR8fJGU8SZtXlsINv5sy3dnuo/ObNyEVxxhHwYRc+lNsaFB04DDkTQId4++eNcTLeA8I7i/uhL7ERqV3gl2mjUOfqKXaOwxc/1D2P0VGsBQ55lEMA9ZfrZMeidBL4Ltw1Rlx9RxBX7NEwH20GfISICI1UWqRcTTGdYjEk4IK4VXulmZVd6wbcY2kfdzyoFDuan4iBou4hkCqV/kJMIxh/vcRoBY/WnVcBwvIYNH2NnIHzs2lvMbLHq8PFtaEBFZrGNVtJIGssxcDJlbpBVHHhElkH4SVjcc6dqhdh1b1XALNrKiboZMnkMNoqxV+ktc8VLlrXJMZQeRupL4uDjESd0T8a3TPtFXv6vi9VxeSztRPwfePlKM9CQnF5rX7AhVwrY262N6P2z0g7RzZnrjk6HcBV+6+tnimVduZs39rEybHZX25DPuKh6vvjHtvLIaYgTAAAAAAAAALnS/wAAAADG+nrzvtutOj1l82qryXQxsbvkwtL24OR8pgIDRS9dYQ==";

    #[test]
    fn global_head_decodes_live_mainnet_account() {
        let data = crate::sol::rpc::b64_decode(LIVE_GLOBAL_B64).expect("base64");
        let g = GlobalHead::decode(&data).expect("decode live Global");

        println!("initialized       = {}", g.initialized);
        println!("authority         = {}", g.authority);
        println!("fee_recipient     = {}", g.fee_recipient);
        println!("fee_basis_points  = {}", g.fee_basis_points);
        println!("fee_frac          = {}", g.fee_frac());
        println!("init_virt_token   = {}", g.initial_virtual_token_reserves);
        println!("init_virt_sol     = {}", g.initial_virtual_sol_reserves);
        println!("init_real_token   = {}", g.initial_real_token_reserves);
        println!("token_total_supply= {}", g.token_total_supply);

        assert!(g.initialized, "live global must be initialized");
        assert_ne!(g.fee_recipient, Pubkey::default(), "fee_recipient must be a real address");
        // A protocol fee outside 0-10% would mean we're reading the wrong offset.
        assert!(g.fee_basis_points > 0 && g.fee_basis_points <= 1000,
            "fee_basis_points {} is not a plausible fee - layout is wrong", g.fee_basis_points);
        // pump.fun's documented launch defaults, as a layout cross-check.
        assert_eq!(g.token_total_supply, 1_000_000_000_000_000, "expected 1B tokens @ 6dp");
        assert_eq!(g.initial_virtual_sol_reserves, 30_000_000_000, "expected 30 SOL virtual");
    }

    /// pump routes part of its fee to a buyback, and rejects any trade that
    /// names no recipient (`BuybackFeeRecipientMissing`, 6062). The field sits
    /// ~740 bytes into `Global`, so this checks the offset math against the
    /// same live account the test above decodes.
    #[test]
    fn buyback_recipient_is_read_from_the_right_offset() {
        let data = crate::sol::rpc::b64_decode(LIVE_GLOBAL_B64).expect("base64");
        let got = GlobalHead::buyback_recipient(&data).expect("a recipient is configured");
        assert_eq!(got.to_string(), "5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeD");
        // Truncation must degrade to None, not read neighbouring fields as a key.
        assert!(GlobalHead::buyback_recipient(&data[..600]).is_none());
        assert!(GlobalHead::buyback_recipient(&[]).is_none());
    }

    /// Every account of a real mainnet `sell_v2`, in order, taken from tx
    /// 3yo9HUMrbTk73je5BbSXVSyCvZ5hz5M9MWh7N4pey3QG. v2 declares all of them,
    /// so if a derivation drifts this fails here instead of in a live trade.
    #[test]
    fn v2_accounts_match_a_live_mainnet_sell() {
        let mint: Pubkey = "6c5eZWM8MSRdr6oYxubXr8hweAPJX7QtQNrpKRRmpump".parse().unwrap();
        let token22: Pubkey = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb".parse().unwrap();
        let creator: Pubkey = "2sK7gtAGoHsBKPkTY8pXnmEBoCYnyPrLrZjBr4SXp96E".parse().unwrap();
        let user: Pubkey = "4YCD7moMd7ecCEH1qBTozAUcCauStipF9sakUrP5DWL6".parse().unwrap();
        let fee: Pubkey = "62qc2CNXwrYqQScmEdiZFFAnJR262PxWEuNQtxfafNgV".parse().unwrap();
        let buyback: Pubkey = "5cjcW9wExnJJiqgLjq7DEG75Pm6JBgE1hNv4B2vHXUW6".parse().unwrap();
        let expected = [
            "4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf", // global
            "6c5eZWM8MSRdr6oYxubXr8hweAPJX7QtQNrpKRRmpump", // base_mint
            "So11111111111111111111111111111111111111112",  // quote_mint
            "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",  // base_token_program
            "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",  // quote_token_program
            "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL", // associated_token_program
            "62qc2CNXwrYqQScmEdiZFFAnJR262PxWEuNQtxfafNgV", // fee_recipient
            "94qWNrtmfn42h3ZjUZwWvK1MEo9uVmmrBPd2hpNjYDjb", // associated_quote_fee_recipient
            "5cjcW9wExnJJiqgLjq7DEG75Pm6JBgE1hNv4B2vHXUW6", // buyback_fee_recipient
            "GYH1Gae1wJytMSvMvw8JVcv7nuAbxi8i9erNVbERnzXd", // associated_quote_buyback
            "DYMxCcWQWZrmH72FKvZiSWrLaFEvyn7kyMxBWt4cmcJ4", // bonding_curve
            "61JEMfpyXVRKwW3ESNcc8HdwnHxdDgvmCn1aSMYmAo2f", // associated_base_bonding_curve
            "AzjNFcet1AFSgeStSmWVChTwdB5Uqub6TQgpJsBAYYGm", // associated_quote_bonding_curve
            "4YCD7moMd7ecCEH1qBTozAUcCauStipF9sakUrP5DWL6", // user
            "F1KTiUd9DfsXeFmxs4JEt2tk1oYJRJFqTxxbhCy6yk2T", // associated_base_user
            "37TbKuYeeUPr89xTz1mUbx4xhgX5XsNfpJXwpR52tZZw", // associated_quote_user
            "91ypQbj94ihkDWDFjSNgiYopVqq9d73tZTwQCjnSboSw", // creator_vault
            "CRnRbsURSCzHyzuwjNPtvQevpb1AYrWVDkaombrw9VGe", // associated_creator_vault
            "9SWQfWXy28Sq5sWWJHwxdCRBeQQsi3juHLoEeR4ut36a", // sharing_config
            "oMUgChf6muhrL3kmzYhLMrWoWmhJpDAkNxvKstcxPcc",  // user_volume_accumulator
            "3Kn4hKzXoU6TfXH9zuwi8aTyFgUNDhLgcyue9v1YBySL", // associated_user_volume_accumulator
            "8Wf5TiAheLUqBrKXeYg2JtAFFMWtKdG2BSFgqUcPVwTt", // fee_config
            "pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ",  // fee_program
            "11111111111111111111111111111111",             // system_program
            "Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1", // event_authority
            "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P",  // program
        ];
        let keys = CurveKeys::new(mint, token22, creator, Pubkey::default());
        let ix = sell_v2_ix(&keys, &user, &fee, &buyback, 1_000, 1);
        assert_eq!(ix.accounts.len(), expected.len(), "sell_v2 account count");
        for (i, want) in expected.iter().enumerate() {
            assert_eq!(&ix.accounts[i].pubkey.to_string(), want, "sell_v2 account {i}");
        }
        assert!(ix.accounts[13].is_signer, "the user signs");
        assert!(!ix.accounts[0].is_writable, "global stays read-only");
        assert_eq!(&ix.data[..8], &DISC_SELL_V2);

        // The buy form is the same list plus global_volume_accumulator at 19.
        let buy = buy_exact_quote_in_v2_ix(&keys, &user, &fee, &buyback, 1_000, 1);
        assert_eq!(buy.accounts.len(), expected.len() + 1);
        assert_eq!(buy.accounts[19].pubkey, super::super::global_volume_accumulator_pda());
        assert!(!buy.accounts[19].is_writable, "shared accumulator takes no write lock");
        assert_eq!(&buy.data[..8], &DISC_BUY_EXACT_QUOTE_IN_V2);
    }

    /// Dumps our v2 account list for a given coin so it can be diffed against
    /// pump's own SDK. Not a check in itself — run with
    /// `MINT=.. CREATOR=.. USER=.. TOKENPROG=.. cargo test print_v2 -- --ignored --nocapture`.
    /// A USDC-quoted coin must derive every quote-side account from USDC, not
    /// from wrapped SOL. Getting this wrong is invisible until you trade one of
    /// those coins, and then every account from index 7 on is wrong at once.
    #[test]
    fn a_non_sol_quoted_coin_derives_quote_accounts_from_its_own_mint() {
        let usdc: Pubkey = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".parse().unwrap();
        let mint = Pubkey::new_from_array([7u8; 32]);
        let user = Pubkey::new_from_array([3u8; 32]);
        let fee = Pubkey::new_from_array([4u8; 32]);
        let buyback = Pubkey::new_from_array([5u8; 32]);
        let creator = Pubkey::new_from_array([9u8; 32]);

        let sol_keys = CurveKeys::new(mint, TOKEN_PROGRAM, creator, Pubkey::default());
        let usdc_keys = CurveKeys::new(mint, TOKEN_PROGRAM, creator, usdc);
        assert_eq!(sol_keys.quote_mint, NATIVE_MINT, "default quote mint means SOL");

        let sol = buy_exact_quote_in_v2_ix(&sol_keys, &user, &fee, &buyback, 1, 1);
        let usd = buy_exact_quote_in_v2_ix(&usdc_keys, &user, &fee, &buyback, 1, 1);
        assert_eq!(sol.accounts[2].pubkey, NATIVE_MINT);
        assert_eq!(usd.accounts[2].pubkey, usdc, "quote_mint account");
        assert_eq!(usd.accounts[15].pubkey, ata(&user, &usdc, &TOKEN_PROGRAM), "assoc_quote_user");
        // Every quote-side account must move; the base-side ones must not.
        for i in [7usize, 9, 12, 15, 17, 21] {
            assert_ne!(sol.accounts[i].pubkey, usd.accounts[i].pubkey, "quote account {i}");
        }
        for i in [1usize, 10, 11, 14, 16, 18] {
            assert_eq!(sol.accounts[i].pubkey, usd.accounts[i].pubkey, "base account {i}");
        }
    }

    /// The fee the curve charges, read off the live account. If this drifts,
    /// every slippage floor drifts with it.
    #[test]
    fn curve_fees_are_read_and_total_one_percent() {
        let data = crate::sol::rpc::b64_decode(LIVE_GLOBAL_B64).expect("base64");
        let g = GlobalHead::decode(&data).unwrap();
        let creator = GlobalHead::creator_fee_frac(&data).expect("creator fee present");
        assert!((g.fee_frac() - 0.0095).abs() < 1e-9, "protocol fee is 95 bps");
        assert!((creator - 0.0005).abs() < 1e-9, "creator fee is 5 bps");
        assert!((g.fee_frac() + creator - 0.01).abs() < 1e-9, "1% all-in");
        assert!(GlobalHead::creator_fee_frac(&data[..100]).is_none());
    }

    /// Mayhem coins bill a separate recipient set, so the two must not collide.
    #[test]
    fn mayhem_coins_use_a_different_fee_recipient() {
        let data = crate::sol::rpc::b64_decode(LIVE_GLOBAL_B64).expect("base64");
        let normal = GlobalHead::decode(&data).unwrap().fee_recipient;
        let mayhem = GlobalHead::reserved_fee_recipient(&data).expect("reserved recipient set");
        assert_ne!(mayhem, normal, "mayhem must bill its own recipient");
        assert_ne!(mayhem, Pubkey::default());
        assert!(GlobalHead::reserved_fee_recipient(&data[..400]).is_none());
    }

    #[test]
    #[ignore]
    fn print_v2_accounts_for_diff() {
        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("set {k}"));
        let mint: Pubkey = env("MINT").parse().unwrap();
        let creator: Pubkey = env("CREATOR").parse().unwrap();
        let user: Pubkey = env("USER").parse().unwrap();
        let tp: Pubkey = env("TOKENPROG").parse().unwrap();
        let fee: Pubkey = env("FEE").parse().unwrap();
        let buyback: Pubkey = env("BUYBACK").parse().unwrap();
        let ix = buy_exact_quote_in_v2_ix(
            &CurveKeys::new(mint, tp, creator, Pubkey::default()), &user, &fee, &buyback, 390_000, 1,
        );
        for (i, a) in ix.accounts.iter().enumerate() {
            let w = if a.is_writable { 'w' } else { 'r' };
            let s = if a.is_signer { 's' } else { ' ' };
            println!("RUST {i:2} [{w}{s}] {}", a.pubkey);
        }
    }

    #[test]
    fn wrap_helpers_encode_the_spl_abi() {
        let (a, b) = (Pubkey::new_from_array([1u8; 32]), Pubkey::new_from_array([2u8; 32]));
        let t = transfer_lamports(&a, &b, 585_000);
        assert_eq!(t.program_id, SYSTEM_PROGRAM);
        assert_eq!(&t.data[..4], &2u32.to_le_bytes(), "system Transfer is tag 2");
        assert_eq!(&t.data[4..], &585_000u64.to_le_bytes());
        assert!(t.accounts[0].is_signer && t.accounts[0].is_writable);

        let sy = sync_native(&a, &TOKEN_PROGRAM);
        assert_eq!(sy.data, vec![17], "SyncNative is tag 17");
        assert!(sy.accounts[0].is_writable);

        let c = close_token_account(&a, &b, &b, &TOKEN_PROGRAM);
        assert_eq!(c.data, vec![9], "CloseAccount is tag 9");
        assert!(c.accounts[2].is_signer, "the owner must sign a close");
    }
}
