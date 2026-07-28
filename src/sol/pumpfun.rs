//! pump.fun bonding-curve state and pricing.
//!
//! One account — `BondingCurve` — drives everything: the trenches list (price,
//! pooled SOL, graduated flag) and the trade math (quotes, slippage caps). Its
//! layout comes from the official IDL (`../pump-public-docs/idl/pump.json`) and
//! is cross-checked against `../carbon/decoders/pumpfun-decoder`.
//!
//! The curve is a constant product over *virtual* reserves: `k = vt * vq`. The
//! virtual reserves are seeded above the real ones so early price is finite;
//! `real_*` is what can actually be withdrawn.

use borsh::BorshDeserialize;
use solana_pubkey::Pubkey;

use super::{lamports_to_sol, units_to_tokens};

/// Anchor discriminator for the `BondingCurve` account.
const BONDING_CURVE_DISC: [u8; 8] = [23, 183, 248, 55, 96, 216, 172, 96];

/// On-chain bonding-curve state. Field order is the wire layout — do not reorder.
#[derive(Debug, Clone, BorshDeserialize)]
pub struct BondingCurve {
    pub virtual_token_reserves: u64,
    pub virtual_quote_reserves: u64,
    pub real_token_reserves: u64,
    pub real_quote_reserves: u64,
    pub token_total_supply: u64,
    /// True once the coin has graduated off the curve to the AMM.
    pub complete: bool,
    pub creator: Pubkey,
    pub is_mayhem_mode: bool,
    pub is_cashback_coin: bool,
    /// `Pubkey::default()` for SOL-paired coins.
    pub quote_mint: Pubkey,
}

impl BondingCurve {
    /// Decode raw account data, verifying the discriminator so we can't mistake
    /// some other account for a curve.
    pub fn decode(data: &[u8]) -> eyre::Result<BondingCurve> {
        if data.len() < 8 {
            eyre::bail!("bonding curve account too short ({} bytes)", data.len());
        }
        if data[..8] != BONDING_CURVE_DISC {
            eyre::bail!("not a BondingCurve account (discriminator mismatch)");
        }
        let mut rest = &data[8..];
        BondingCurve::deserialize(&mut rest).map_err(|e| eyre::eyre!("bonding curve decode: {e}"))
    }

    /// Price in SOL per whole token. Decimals differ (SOL 9, token 6), so this
    /// converts both sides rather than dividing raw reserves.
    pub fn price_sol(&self) -> f64 {
        let vt = units_to_tokens(self.virtual_token_reserves);
        let vq = lamports_to_sol(self.virtual_quote_reserves);
        if vt > 0.0 { vq / vt } else { 0.0 }
    }

    /// Fully-diluted market cap in SOL (total supply × price).
    pub fn market_cap_sol(&self) -> f64 {
        units_to_tokens(self.token_total_supply) * self.price_sol()
    }

    /// SOL actually pooled in the curve — the real exit liquidity, not the
    /// virtual figure. This is the number that matters when a pool is draining.
    pub fn pooled_sol(&self) -> f64 {
        lamports_to_sol(self.real_quote_reserves)
    }

    /// Tokens still buyable from the curve.
    pub fn available_tokens(&self) -> f64 {
        units_to_tokens(self.real_token_reserves)
    }

    /// How far along the curve this coin is, 0.0–1.0 — the "bonding progress"
    /// pump.fun shows. Graduation happens as real token reserves are exhausted.
    pub fn progress(&self) -> f64 {
        let total = self.token_total_supply as f64;
        if total <= 0.0 {
            return 0.0;
        }
        (1.0 - self.real_token_reserves as f64 / total).clamp(0.0, 1.0)
    }

    /// Tokens received for `sol_in` SOL, ignoring fees (constant product on the
    /// virtual reserves). Fees are charged on the SOL side by the program, so a
    /// real fill is slightly smaller — always pair this with a slippage cap.
    pub fn tokens_out(&self, sol_in: f64) -> f64 {
        if sol_in <= 0.0 {
            return 0.0;
        }
        let vt = self.virtual_token_reserves as f64;
        let vq = self.virtual_quote_reserves as f64;
        let dq = sol_in * super::LAMPORTS_PER_SOL as f64;
        let k = vt * vq;
        let new_vt = k / (vq + dq);
        let out_units = (vt - new_vt).max(0.0);
        // Can't buy more than the curve actually holds.
        units_to_tokens(out_units.min(self.real_token_reserves as f64) as u64)
    }

    /// SOL received for selling `tokens_in` whole tokens, ignoring fees.
    pub fn sol_out(&self, tokens_in: f64) -> f64 {
        if tokens_in <= 0.0 {
            return 0.0;
        }
        let vt = self.virtual_token_reserves as f64;
        let vq = self.virtual_quote_reserves as f64;
        let dt = tokens_in * 10f64.powi(super::TOKEN_DECIMALS as i32);
        let k = vt * vq;
        let new_vq = k / (vt + dt);
        let out_lamports = (vq - new_vq).max(0.0);
        // Can't take out more real SOL than is pooled.
        lamports_to_sol(out_lamports.min(self.real_quote_reserves as f64) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// pump.fun's launch defaults: 1.073e15 token units (1.073B tokens @ 6dp)
    /// virtual, 30 SOL virtual quote, 793.1M real tokens, 1B total supply.
    fn fresh_curve() -> BondingCurve {
        BondingCurve {
            virtual_token_reserves: 1_073_000_000_000_000,
            virtual_quote_reserves: 30_000_000_000,
            real_token_reserves: 793_100_000_000_000,
            real_quote_reserves: 0,
            token_total_supply: 1_000_000_000_000_000,
            complete: false,
            creator: Pubkey::default(),
            is_mayhem_mode: false,
            is_cashback_coin: false,
            quote_mint: Pubkey::default(),
        }
    }

    #[test]
    fn decode_rejects_wrong_discriminator() {
        let mut data = vec![0u8; 200];
        data[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(BondingCurve::decode(&data).is_err());
        assert!(BondingCurve::decode(&[0u8; 4]).is_err(), "short data must error");
    }

    #[test]
    fn price_and_market_cap_are_consistent() {
        let c = fresh_curve();
        // 30 SOL / 1.073B tokens ~= 2.8e-8 SOL per token.
        let p = c.price_sol();
        assert!(p > 2.7e-8 && p < 2.9e-8, "unexpected launch price {p}");
        // Cap = 1B tokens * price ~= 27.96 SOL.
        let mc = c.market_cap_sol();
        assert!(mc > 27.0 && mc < 29.0, "unexpected launch mkt cap {mc}");
    }

    #[test]
    fn buying_moves_price_up_and_is_bounded() {
        let c = fresh_curve();
        let small = c.tokens_out(0.1);
        let big = c.tokens_out(1.0);
        assert!(small > 0.0 && big > small, "more SOL must buy more tokens");
        // Constant product: 10x the SOL buys LESS than 10x the tokens (slippage).
        assert!(big < small * 10.0, "curve must exhibit slippage");
        // Never sells more than the curve holds.
        assert!(c.tokens_out(1e9) <= c.available_tokens() + 1.0);
    }

    #[test]
    fn sell_is_capped_by_real_reserves() {
        let mut c = fresh_curve();
        // A fresh curve holds no real SOL, so nothing can be taken out.
        assert_eq!(c.sol_out(1_000_000.0), 0.0);
        // With real SOL pooled, a sell returns some but never more than pooled.
        c.real_quote_reserves = 5_000_000_000; // 5 SOL
        let out = c.sol_out(1_000_000.0);
        assert!(out > 0.0 && out <= c.pooled_sol(), "sell out {out} vs pooled {}", c.pooled_sol());
    }

    #[test]
    fn progress_tracks_token_depletion() {
        let mut c = fresh_curve();
        let start = c.progress();
        c.real_token_reserves = c.token_total_supply / 2;
        assert!(c.progress() > start);
        c.real_token_reserves = 0;
        assert_eq!(c.progress(), 1.0, "exhausted curve is 100% bonded");
    }
}
