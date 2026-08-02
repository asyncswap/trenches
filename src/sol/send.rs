// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! Sending SOL or a token to someone else.
//!
//! Every instruction here is built in this file. There is no router, no quote
//! and no remote service — a transfer is the one operation where the whole
//! transaction is knowable in advance, and it should stay that way.
//!
//! It is also the one operation with no undo. A swap that goes wrong leaves you
//! holding something; a transfer to the wrong address leaves you holding
//! nothing, with no counterparty and no appeal. So the checks below are not
//! ceremony: they are the only thing standing between a mistyped character and
//! a permanent loss, and each one exists because that mistake is easy to make.

use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;

use super::{ata, SYSTEM_PROGRAM, TOKEN_PROGRAM};

/// What a send would do, resolved and checked, before anything is signed.
pub struct Plan {
    pub to: Pubkey,
    /// None = native SOL.
    pub mint: Option<Pubkey>,
    pub symbol: String,
    /// Base units — lamports for SOL, the mint's own units for a token.
    pub amount: u64,
    pub decimals: u32,
    /// The recipient has no account for this token yet, so the send creates
    /// one and pays its rent. Worth saying out loud: it costs the SENDER about
    /// 0.002 SOL, and nothing on the screen would otherwise explain where that
    /// went.
    pub creates_account: bool,
}

impl Plan {
    /// The amount in whole tokens, for showing a person.
    pub fn ui_amount(&self) -> f64 {
        self.amount as f64 / 10f64.powi(self.decimals as i32)
    }

    /// One line, in words, for the confirmation. Deliberately spells out the
    /// FULL destination: an abbreviated address is exactly as reassuring for
    /// the right one as for an attacker's lookalike, and this is the moment
    /// that distinction is worth the width.
    pub fn sentence(&self) -> String {
        let extra = if self.creates_account {
            "  (creates their token account — costs you about 0.002 SOL)"
        } else {
            ""
        };
        format!("Send {} {} to {}{}", self.ui_amount(), self.symbol, self.to, extra)
    }
}

/// Why a destination was refused.
#[derive(Debug, PartialEq)]
pub enum Refusal {
    NotAnAddress,
    Yourself,
    TheMint,
    AProgram,
}

impl Refusal {
    pub fn say(&self) -> &'static str {
        match self {
            Refusal::NotAnAddress => "That is not a Solana address.",
            Refusal::Yourself => "That is this wallet's own address.",
            // The single most common way tokens are destroyed: pasting the
            // token's mint instead of the recipient. Nobody controls a mint
            // address, so the tokens simply stop existing for anyone.
            Refusal::TheMint => {
                "That is the token's own mint address, not a wallet. Tokens sent there are gone."
            }
            Refusal::AProgram => {
                "That address is a program, not a wallet. Anything sent there is unrecoverable."
            }
        }
    }
}

/// Check a destination before it is ever used to build anything.
///
/// `executable` comes from the chain — a program account cannot receive and
/// nobody can retrieve from it, so it is refused rather than warned about.
pub fn check_destination(
    to: &str,
    me: &Pubkey,
    mint: Option<&Pubkey>,
    executable: bool,
) -> Result<Pubkey, Refusal> {
    let to: Pubkey = to.trim().parse().map_err(|_| Refusal::NotAnAddress)?;
    if to == *me {
        return Err(Refusal::Yourself);
    }
    if Some(&to) == mint {
        return Err(Refusal::TheMint);
    }
    if executable {
        return Err(Refusal::AProgram);
    }
    Ok(to)
}

/// Move native SOL.
pub fn transfer_sol(from: &Pubkey, to: &Pubkey, lamports: u64) -> Instruction {
    // System program transfer: instruction index 2, then the amount.
    let mut data = Vec::with_capacity(12);
    data.extend_from_slice(&2u32.to_le_bytes());
    data.extend_from_slice(&lamports.to_le_bytes());
    Instruction {
        program_id: SYSTEM_PROGRAM,
        accounts: vec![AccountMeta::new(*from, true), AccountMeta::new(*to, false)],
        data,
    }
}

/// Move an SPL token, creating the recipient's account if they have none.
///
/// `TransferChecked` rather than `Transfer`: it carries the mint and the
/// decimals, and the token program verifies both. A plain transfer will happily
/// move 1000 base units when 1000 whole tokens were meant, and the difference
/// between those is a factor of a million on a 6-decimal mint.
/// `program` is the mint's OWN token program — classic or Token-2022. It is
/// passed in rather than assumed because it decides the associated-account
/// address as well as who executes the transfer, so guessing it wrong builds a
/// correct-looking instruction against an account nobody owns.
pub fn transfer_token(
    from: &Pubkey,
    to: &Pubkey,
    mint: &Pubkey,
    amount: u64,
    decimals: u8,
    create_ata: bool,
    program: &Pubkey,
) -> Vec<Instruction> {
    let src = ata(from, mint, program);
    let dst = ata(to, mint, program);
    let mut ixs = Vec::with_capacity(2);
    if create_ata {
        ixs.push(super::trade::create_ata_idempotent(from, to, mint, program));
    }
    let mut data = Vec::with_capacity(10);
    data.push(12u8); // TransferChecked
    data.extend_from_slice(&amount.to_le_bytes());
    data.push(decimals);
    ixs.push(Instruction {
        program_id: *program,
        accounts: vec![
            AccountMeta::new(src, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(dst, false),
            AccountMeta::new_readonly(*from, true),
        ],
        data,
    });
    ixs
}

/// The most SOL that can be sent while leaving the account able to function.
///
/// Sending the whole balance leaves nothing for the fee on the very
/// transaction that sends it, and nothing for the next one either — a wallet
/// that cannot pay a fee cannot move its own tokens, so an "everything" that
/// empties it is a wallet locked with its contents inside.
pub fn max_sol_lamports(balance: u64) -> u64 {
    // Enough for this fee and a handful after it.
    const KEEP: u64 = 5_000_000; // 0.005 SOL
    balance.saturating_sub(KEEP)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(b: u8) -> Pubkey {
        Pubkey::new_from_array([b; 32])
    }

    #[test]
    fn a_transfer_to_the_mint_is_refused() {
        let (me, mint) = (key(1), key(2));
        assert_eq!(
            check_destination(&mint.to_string(), &me, Some(&mint), false),
            Err(Refusal::TheMint)
        );
    }

    #[test]
    fn a_transfer_to_a_program_is_refused() {
        let (me, other) = (key(1), key(3));
        assert_eq!(
            check_destination(&other.to_string(), &me, None, true),
            Err(Refusal::AProgram)
        );
    }

    #[test]
    fn sending_to_yourself_is_refused() {
        let me = key(1);
        assert_eq!(check_destination(&me.to_string(), &me, None, false), Err(Refusal::Yourself));
    }

    #[test]
    fn nonsense_is_not_an_address() {
        let me = key(1);
        assert_eq!(check_destination("not-a-key", &me, None, false), Err(Refusal::NotAnAddress));
    }

    #[test]
    fn an_ordinary_wallet_is_accepted() {
        let (me, them) = (key(1), key(9));
        assert_eq!(check_destination(&them.to_string(), &me, Some(&key(2)), false), Ok(them));
    }

    /// Emptying the account would leave it unable to pay the fee for its own
    /// next transaction — including the one that would move its tokens out.
    #[test]
    fn sending_everything_still_leaves_the_account_usable() {
        let bal = 1_000_000_000; // 1 SOL
        let max = max_sol_lamports(bal);
        assert!(max < bal, "something is held back");
        assert!(bal - max >= 1_000_000, "and it is enough to pay a fee");
        // A balance already below the reserve sends nothing rather than
        // underflowing into an enormous amount.
        assert_eq!(max_sol_lamports(1_000), 0);
    }

    /// TransferChecked carries the decimals so the token program can reject a
    /// mismatch. The discriminator and layout are what make that true.
    #[test]
    fn the_token_transfer_is_the_checked_one() {
        let ixs = transfer_token(&key(1), &key(2), &key(3), 1_500_000, 6, false, &TOKEN_PROGRAM);
        let ix = ixs.last().unwrap();
        assert_eq!(ix.data[0], 12, "TransferChecked");
        assert_eq!(u64::from_le_bytes(ix.data[1..9].try_into().unwrap()), 1_500_000);
        assert_eq!(ix.data[9], 6, "decimals travel with the amount");
    }

    #[test]
    fn creating_the_recipients_account_adds_an_instruction_before_the_transfer() {
        let with = transfer_token(&key(1), &key(2), &key(3), 1, 6, true, &TOKEN_PROGRAM);
        let without = transfer_token(&key(1), &key(2), &key(3), 1, 6, false, &TOKEN_PROGRAM);
        assert_eq!(with.len(), without.len() + 1);
        assert_eq!(with.last().unwrap().data[0], 12, "the transfer stays last");
    }

    /// A Token-2022 mint is executed by its own program and its associated
    /// account is derived against that program — pump.fun issues these, so
    /// assuming the classic one targets an account that does not exist.
    #[test]
    fn a_token_2022_transfer_uses_its_own_program() {
        let t22 = super::super::TOKEN_2022_PROGRAM;
        let ixs = transfer_token(&key(1), &key(2), &key(3), 1, 6, false, &t22);
        assert_eq!(ixs.last().unwrap().program_id, t22);
        let classic = transfer_token(&key(1), &key(2), &key(3), 1, 6, false, &TOKEN_PROGRAM);
        assert_ne!(
            ixs.last().unwrap().accounts[0].pubkey,
            classic.last().unwrap().accounts[0].pubkey,
            "and a different source account"
        );
    }

    #[test]
    fn a_sol_transfer_moves_what_it_says() {
        let ix = transfer_sol(&key(1), &key(2), 12_345);
        assert_eq!(ix.program_id, SYSTEM_PROGRAM);
        assert_eq!(u32::from_le_bytes(ix.data[0..4].try_into().unwrap()), 2);
        assert_eq!(u64::from_le_bytes(ix.data[4..12].try_into().unwrap()), 12_345);
    }
}
