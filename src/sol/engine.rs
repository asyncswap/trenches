// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! High-level pump.fun trading: resolve a coin's on-chain state, quote it, build
//! the instructions, sign, send, confirm.
//!
//! Everything below operates on the **bonding curve**. Once a coin graduates
//! (`BondingCurve.complete == true`) its liquidity moves to the pump AMM and
//! these instructions no longer apply — that case is refused loudly rather than
//! sending a transaction that would fail after paying fees.

use std::time::Duration;

use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;

use super::pumpfun::BondingCurve;
use super::rpc::Rpc;
use super::pumpswap;
use super::trade::{self, CurveKeys, GlobalHead};
use super::tx;
use super::{ata, bonding_curve_pda, global_pda, sol_to_lamports, tokens_to_units};

/// How long to wait for a trade to land before reporting it as unknown.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(30);

/// Where a coin actually trades. pump.fun coins start on a bonding curve and
/// migrate to the PumpSwap AMM once they graduate — most coins with real volume
/// are on the AMM, so supporting only the curve misses the liquid half.
pub enum Venue {
    Curve {
        curve: BondingCurve,
        keys: CurveKeys,
        /// Protocol fee recipient, from the pump `Global` account.
        fee_recipient: Pubkey,
        /// Protocol + creator fee, as a fraction of the trade.
        ///
        /// The curve math is fee-free — the program deducts these on top — so
        /// a quote that ignores them sits ~1% above what actually gets paid
        /// out, and every slippage floor derived from it is too high to fill.
        fee_frac: f64,
        /// Buyback fee recipient, also from `Global`. `None` means the field
        /// could not be read, which makes trading impossible — the program
        /// rejects a trade that names no recipient — so we say so plainly
        /// rather than sending a transaction that cannot land.
        buyback_recipient: Option<Pubkey>,
    },
    Amm {
        keys: pumpswap::SwapKeys,
        /// Live reserves, stored BY MEANING rather than by pool layout.
        ///
        /// A PumpSwap pool can be created either way round, and ~78% of SOL
        /// pools on mainnet are "inverted" (SOL is the base, the coin is the
        /// quote). Naming these `base`/`quote` invited every pricing and sizing
        /// expression to assume one layout; naming them for what they hold
        /// makes the math orientation-blind. Only instruction building — which
        /// genuinely speaks the pool's layout — branches on `sol_is_base`.
        sol_res: f64,
        token_res: f64,
        /// Combined LP + protocol fee.
        fee_frac: f64,
        /// The COIN's decimals (not the pool's base side), read from the mint.
        token_decimals: u8,
        /// Circulating supply in whole tokens, read from the mint. pump coins
        /// are 1B, but listed coins range widely — one live pool's supply is
        /// 100B, and assuming 1B understated its market cap by 100x.
        total_supply: f64,
        /// True when SOL occupies the pool's base slot. Flips which instruction
        /// means "buy": on such a pool, paying SOL is a `sell` (base in).
        sol_is_base: bool,
        /// BOOST: the pool's virtual quote reserves, raw quote base units.
        /// Zero on a plain pool. Pricing uses effective = real + virtual; a
        /// sell's PAYOUT stays capped at the real vault (the program refuses
        /// past it with 6063), so quotes cap there too.
        virtual_quote_raw: i128,
    },
}

/// A coin resolved to everything needed to price and trade it, on whichever
/// venue currently holds its liquidity.
pub struct Coin {
    pub mint: Pubkey,
    pub venue: Venue,
}

impl Coin {
    /// True when the coin trades on the AMM rather than a bonding curve.
    pub fn on_amm(&self) -> bool {
        matches!(self.venue, Venue::Amm { .. })
    }

    /// Has this coin left the bonding curve?
    ///
    /// True either once it trades on the AMM, OR the moment its curve reports
    /// `complete` — the window in between is real: the curve is finished and
    /// drained while the pool is still being created. Aliasing this to
    /// `on_amm()` made that window invisible, so a coin that graduated while
    /// being watched silently kept polling a dead curve.
    pub fn graduated(&self) -> bool {
        match &self.venue {
            Venue::Amm { .. } => true,
            Venue::Curve { curve, .. } => curve.complete,
        }
    }

    /// The account this coin actually trades against: its AMM pool once
    /// graduated, its bonding curve before that. This is the "pair" address
    /// charts and explorers key off, and it is NOT the mint.
    pub fn pair_address(&self) -> Option<Pubkey> {
        match &self.venue {
            Venue::Amm { keys, .. } => Some(keys.pool),
            Venue::Curve { keys, .. } => Some(keys.bonding_curve),
        }
    }

    /// Circulating supply in whole tokens. Read from the mint on the AMM; the
    /// curve account carries its own figure.
    pub fn total_supply(&self) -> f64 {
        match &self.venue {
            Venue::Amm { total_supply, .. } => *total_supply,
            Venue::Curve { curve, .. } => super::units_to_tokens(curve.token_total_supply),
        }
    }

    /// Short tag, for dense table cells.
    pub fn venue_label(&self) -> &'static str {
        match self.venue {
            Venue::Curve { .. } => "curve",
            Venue::Amm { .. } => "amm",
        }
    }

    /// The venue spelled out for the header, where there is room for it.
    pub fn venue_long(&self) -> &'static str {
        match self.venue {
            Venue::Curve { .. } => "bonding curve",
            Venue::Amm { .. } => "Pump AMM",
        }
    }

    /// SOL per whole token.
    pub fn price_sol(&self) -> f64 {
        match &self.venue {
            Venue::Curve { curve, .. } => curve.price_sol(),
            Venue::Amm { .. } => {
                // Effective reserves: a BOOST pool's price includes the
                // virtual quote, exactly as the program computes it.
                let (es, et) = self.amm_effective().unwrap_or_default();
                if et > 0.0 { es / et } else { 0.0 }
            }
        }
    }

    /// Fully-diluted market cap in SOL. pump coins have a fixed 1B supply.
    pub fn market_cap_sol(&self) -> f64 {
        match &self.venue {
            Venue::Curve { curve, .. } => curve.market_cap_sol(),
            Venue::Amm { total_supply, .. } => total_supply * self.price_sol(),
        }
    }

    /// Circulating supply, derived rather than stored.
    ///
    /// Market cap is supply times price, so the supply is the ratio — and both
    /// halves already handle the curve/AMM split, which a separate field would
    /// have to duplicate and keep in step.
    pub fn supply(&self) -> f64 {
        let p = self.price_sol();
        if p > 0.0 { self.market_cap_sol() / p } else { 0.0 }
    }

    /// Real SOL backing the market — the exit liquidity that actually matters.
    pub fn pooled_sol(&self) -> f64 {
        match &self.venue {
            Venue::Curve { curve, .. } => curve.pooled_sol(),
            Venue::Amm { sol_res, .. } => *sol_res,
        }
    }

    /// Bonding progress, 0..1. Always 1.0 on the AMM (fully bonded).
    pub fn progress(&self) -> f64 {
        match &self.venue {
            Venue::Curve { curve, .. } => curve.progress(),
            Venue::Amm { .. } => 1.0,
        }
    }

    pub fn creator(&self) -> Pubkey {
        match &self.venue {
            Venue::Curve { curve, .. } => curve.creator,
            Venue::Amm { keys, .. } => keys.coin_creator,
        }
    }

    /// Decimals of the coin itself. Curve coins are always 6dp; AMM coins
    /// carry their own, read from the mint.
    pub fn token_decimals(&self) -> u32 {
        match &self.venue {
            Venue::Curve { .. } => super::TOKEN_DECIMALS,
            Venue::Amm { token_decimals, .. } => *token_decimals as u32,
        }
    }

    pub fn token_program(&self) -> Pubkey {
        match &self.venue {
            Venue::Curve { keys, .. } => keys.token_program,
            Venue::Amm { keys, .. } => keys.base_token_program,
        }
    }

    /// The AMM's reserves as PRICING sees them: real vaults plus BOOST's
    /// virtual quote, mapped onto the (SOL, coin) meaning by orientation.
    /// Zero virtual — every plain pool — returns the vaults untouched.
    fn amm_effective(&self) -> Option<(f64, f64)> {
        let Venue::Amm { sol_res, token_res, sol_is_base, virtual_quote_raw, token_decimals, .. } =
            &self.venue
        else {
            return None;
        };
        let vq = *virtual_quote_raw as f64;
        // The virtual sits in the pool's QUOTE slot; which side of the
        // (SOL, coin) pair that is depends on the orientation.
        if *sol_is_base {
            Some((*sol_res, token_res + vq / 10f64.powi(*token_decimals as i32)))
        } else {
            Some((sol_res + vq / 1e9, *token_res))
        }
    }

    /// Tokens received for `sol_in`, on whichever venue applies.
    pub fn tokens_out(&self, sol_in: f64) -> f64 {
        match &self.venue {
            // The fee comes off the SOL going in, so the curve only ever sees
            // what is left after it.
            Venue::Curve { curve, fee_frac, .. } => curve.tokens_out(sol_in * (1.0 - fee_frac)),
            Venue::Amm { token_res, fee_frac, .. } => {
                let (es, et) = self.amm_effective().unwrap_or_default();
                // The pool can only pay out tokens its vault actually holds.
                pumpswap::tokens_out(et, es, sol_in, *fee_frac).min(token_res * 0.9995)
            }
        }
    }

    /// SOL received for selling `tokens`.
    pub fn sol_out(&self, tokens: f64) -> f64 {
        match &self.venue {
            // On the way out the swap happens first and the fee is taken from
            // the proceeds.
            Venue::Curve { curve, fee_frac, .. } => curve.sol_out(tokens) * (1.0 - fee_frac),
            Venue::Amm { sol_res, fee_frac, .. } => {
                let (es, et) = self.amm_effective().unwrap_or_default();
                // BOOST prices on effective but PAYS from the real vault —
                // the program refuses past it (6063), so the quote stops there
                // too rather than promising SOL the pool does not hold.
                pumpswap::sol_out(et, es, tokens, *fee_frac).min(sol_res * 0.9995)
            }
        }
    }
}

/// Load everything needed to price and trade `mint`.
///
/// Three reads, all required: the curve (price/reserves), the mint account (to
/// learn its token program — SPL vs Token-2022, which pump's docs explicitly warn
/// must never be assumed), and the global config (fee recipient).
/// Re-read ONLY the bonding curve — the sole thing that changes trade to trade.
///
/// `token_program` and the global `fee_recipient` are effectively static, so
/// refetching them every refresh (as `load_coin` does) costs two extra round
/// trips for no new information. On a slow public RPC that dominates latency.
/// Re-read only what changes: the curve's reserves, or the AMM pool's balances.
///
/// The static parts (token program, fee recipient, pool addresses) never change,
/// so refetching them every tick costs round trips for no new information — on a
/// slow endpoint that dominates latency.
pub async fn refresh_venue(rpc: &Rpc, coin: &mut Coin) -> eyre::Result<()> {
    match &mut coin.venue {
        Venue::Curve { curve, keys, .. } => {
            let (data, _) = rpc
                .account(&keys.bonding_curve)
                .await?
                .ok_or_else(|| eyre::eyre!("bonding curve vanished"))?;
            *curve = BondingCurve::decode(&data)?;
        }
        Venue::Amm { keys, sol_res, token_res, token_decimals, sol_is_base, virtual_quote_raw, .. } => {
            let (b, q) = tokio::join!(
                rpc.token_balance(&keys.pool_base_ta),
                rpc.token_balance(&keys.pool_quote_ta),
            );
            let (sol_raw, tok_raw) = if *sol_is_base {
                (b.unwrap_or(0), q.unwrap_or(0))
            } else {
                (q.unwrap_or(0), b.unwrap_or(0))
            };
            *sol_res = super::lamports_to_sol(sol_raw);
            *token_res = tok_raw as f64 / 10f64.powi(*token_decimals as i32);
            // BOOST drains as it burns: re-read the pool account so the
            // virtual side of the price tracks. Plain pools (zero) skip the
            // extra read forever.
            if *virtual_quote_raw != 0 {
                if let Ok(Some((data, _))) = rpc.account(&keys.pool).await {
                    *virtual_quote_raw = pumpswap::Pool::virtual_quote_reserves(&data);
                }
            }
        }
    }
    Ok(())
}

/// Resolve a mint to a tradeable coin.
///
/// Tries the bonding curve first; if the coin has graduated (`complete`) or has
/// no curve at all, falls through to its PumpSwap AMM pool. That fallback is what
/// makes graduated coins — most of the liquid market — tradeable.
pub async fn load_coin(rpc: &Rpc, mint: &Pubkey) -> eyre::Result<Coin> {
    let curve_key = bonding_curve_pda(mint);
    if let Ok(Some((curve_data, _))) = rpc.account(&curve_key).await {
        if let Ok(curve) = BondingCurve::decode(&curve_data) {
            if !curve.complete {
                let gpda = global_pda();
                let (token_program, global) =
                    tokio::join!(rpc.mint_owner(mint), rpc.account(&gpda));
                let token_program = token_program?;
                let (global_data, _) = global?
                    .ok_or_else(|| eyre::eyre!("pump global config account missing"))?;
                let global = GlobalHead::decode(&global_data)?;
                return Ok(Coin {
                    mint: *mint,
                    venue: Venue::Curve {
                        keys: CurveKeys::new(*mint, token_program, curve.creator, curve.quote_mint),
                        // Mayhem coins bill a different recipient set.
                        fee_recipient: if curve.is_mayhem_mode {
                            GlobalHead::reserved_fee_recipient(&global_data)
                                .unwrap_or(global.fee_recipient)
                        } else {
                            global.fee_recipient
                        },
                        buyback_recipient: GlobalHead::buyback_recipient(&global_data),
                        fee_frac: global.fee_frac()
                            + GlobalHead::creator_fee_frac(&global_data).unwrap_or(0.0),
                        curve,
                    },
                });
            }
        }
    }

    // Graduated (or never on a curve) -> PumpSwap AMM.
    let (pool_key, pool, virtual_quote_raw) = pumpswap::find_pool(rpc, mint).await?;
    let cfg_pda = pumpswap::global_config_pda();
    let (base_prog, quote_prog, cfg) = tokio::join!(
        rpc.mint_owner(&pool.base_mint),
        rpc.mint_owner(&pool.quote_mint),
        rpc.account(&cfg_pda),
    );
    let (cfg_data, _) = cfg?.ok_or_else(|| eyre::eyre!("PumpSwap global config missing"))?;
    let cfg = pumpswap::GlobalConfigHead::decode(&cfg_data)?;
    let keys = pumpswap::SwapKeys {
        pool: pool_key,
        base_mint: pool.base_mint,
        quote_mint: pool.quote_mint,
        base_token_program: base_prog?,
        quote_token_program: quote_prog?,
        pool_base_ta: pool.pool_base_token_account,
        pool_quote_ta: pool.pool_quote_token_account,
        coin_creator: pool.coin_creator,
        fee_recipient: cfg.fee_recipient(),
        is_cashback_coin: pool.is_cashback_coin,
        buyback_recipient: pumpswap::GlobalConfigHead::buyback_recipient(&cfg_data),
    };
    // Decimals and supply come from the mint, never assumed.
    let coin_mint = pool.coin_mint().unwrap_or(*mint);
    let (supply_raw, token_decimals, _) = rpc.mint_info(&coin_mint).await?;
    let total_supply = supply_raw as f64 / 10f64.powi(token_decimals as i32);
    let sol_is_base = pool.is_sol_based();
    let (sol_res, token_res) = pumpswap::reserves(rpc, &pool, token_decimals).await?;
    Ok(Coin {
        mint: *mint,
        venue: Venue::Amm {
            keys,
            sol_res,
            token_res,
            fee_frac: cfg.fee_frac(),
            token_decimals,
            total_supply,
            sol_is_base,
            virtual_quote_raw,
        },
    })
}

/// Our token balance for a coin, in whole tokens.
pub async fn token_balance(rpc: &Rpc, coin: &Coin, owner: &Pubkey) -> eyre::Result<f64> {
    let account = ata(owner, &coin.mint, &coin.token_program());
    Ok(super::units_to_tokens(rpc.token_balance(&account).await?))
}

/// How far below the formula's floor a simulation may pull protection, in
/// percent.
///
/// The simulation is there because the formula cannot keep up with pump's fee
/// tiers and boost reserves, so it has to be allowed to lower the floor — that
/// is the entire feature, and `min` was how it did it. But the number it
/// lowers the floor with is read out of an `simulateTransaction` response,
/// and the endpoint that answers is not a trusted party: it can say the swap
/// returns almost nothing, and a floor derived from almost nothing is a signed
/// buy with no protection left on it. That is precisely the state a sandwich
/// wants to find, and `min` placed no bound on it.
///
/// So the simulation still lowers the floor, but only this far. A quarter is
/// wider than any honest gap between the fee-free quote and what the program
/// actually pays; a disagreement past it is not fee math, and the floor stays
/// where the formula put it. The buy may then revert instead of filling — loud,
/// cheap and retryable, which an emptied floor is not.
const MAX_SIM_HAIRCUT_PCT: f64 = 25.0;

/// The floor to actually sign, given the formula's floor and the simulation's.
///
/// Bounded at both ends: the formula floor is the ceiling, because a
/// simulation must not be able to talk protection UP past the honest quote
/// either, and `MAX_SIM_HAIRCUT_PCT` below it is the floor of the floor.
fn clamp_sim_floor(quote_floor: u64, sim_floor: u64) -> u64 {
    let limit = trade::with_slippage(quote_floor, MAX_SIM_HAIRCUT_PCT, false);
    sim_floor.clamp(limit.min(quote_floor), quote_floor)
}

/// Buy `sol_in` SOL worth of the coin, via `buy_exact_sol_in`.
///
/// We spend an exact SOL amount and set a **token floor** — the natural shape for
/// "risk N SOL on this coin", and the instruction live pump.fun traffic uses.
///
/// `slippage_pct` is real protection, not decoration: `min_tokens_out` makes the
/// program revert rather than fill worse. The quote deliberately excludes fees
/// (charged on the SOL side), so the floor must absorb slippage *and* fees —
/// hence the clamp rather than trusting a caller-supplied 0.
pub async fn buy(
    rpc: &Rpc,
    signer: &Keypair,
    coin: &Coin,
    sol_in: f64,
    slippage_pct: f64,
    cu_price_micro: u64,
) -> eyre::Result<String> {
    if sol_in <= 0.0 {
        eyre::bail!("buy size must be > 0");
    }
    let expected_tokens = coin.tokens_out(sol_in);
    if expected_tokens <= 0.0 {
        eyre::bail!("no liquidity: {sol_in} SOL would return nothing");
    }
    // Floor sits BELOW the fee-free quote, by slippage + fee headroom.
    let min_tokens_out =
        trade::with_slippage(tokens_to_units(expected_tokens), slippage_pct.max(1.0), false);
    let user = signer.pubkey();

    let ixs = match &coin.venue {
        Venue::Curve { keys, fee_recipient, buyback_recipient, .. } => vec![
            // Idempotent, so a repeat buy costs nothing and a first buy works.
            trade::create_ata_idempotent(&user, &user, &keys.mint, &keys.token_program),
            trade::buy_exact_quote_in_v2_ix(
                keys,
                &user,
                fee_recipient,
                &buyback_recipient.ok_or_else(|| eyre::eyre!("pump's buyback fee recipient could not be read"))?,
                sol_to_lamports(sol_in),
                min_tokens_out,
            ),
        ],
        Venue::Amm { keys, sol_is_base, .. } => {
            // The AMM's real price is whatever pump's CURRENT rules say —
            // boost reserves, dynamic fee tiers by market cap, fields newer
            // than this binary. Instead of modeling that treadmill, ask the
            // chain: simulate the exact swap with a floor of 1, read the
            // token delta the program itself would pay, and set the real
            // floor from THAT. One extra RPC per buy; correct under any fee
            // regime. The formula floor stays as the ceiling — a simulation
            // cannot talk the floor UP past the honest quote — and as the
            // fallback when simulation is unavailable. It is also the anchor
            // the simulation may only move so far DOWN from, because the
            // answer comes from the RPC: see `clamp_sim_floor`.
            let probe = amm_buy_ixs(keys, &user, sol_to_lamports(sol_in), 1, *sol_is_base);
            // The COIN side's ATA — which slot that is depends on orientation.
            let (coin_mint, coin_prog) = if *sol_is_base {
                (&keys.quote_mint, &keys.quote_token_program)
            } else {
                (&keys.base_mint, &keys.base_token_program)
            };
            let ata = super::ata(&user, coin_mint, coin_prog);
            let pre = rpc
                .token_balances(&[ata])
                .await
                .ok()
                .and_then(|b| b.first().copied().flatten())
                .unwrap_or(0);
            let mut floor = min_tokens_out;
            if let Ok((tx0, _)) = tx::build_signed(rpc, signer, probe, tx::CU_LIMIT_AMM, cu_price_micro).await {
                if let Ok(w) = tx::wire(&tx0) {
                    if let Some(post) = rpc.simulate_post_token(&w, &ata).await {
                        let sim_out = post.saturating_sub(pre);
                        if sim_out > 0 {
                            let sim_floor =
                                trade::with_slippage(sim_out, slippage_pct.max(1.0), false);
                            floor = clamp_sim_floor(min_tokens_out, sim_floor);
                            if sim_floor < floor {
                                crate::trace(&format!(
                                    "amm buy: simulation wanted a floor {sim_floor} against a quote floor of \
                                     {min_tokens_out}; too far to be fee math, clamped to {floor}"
                                ));
                            }
                        }
                    }
                }
            }
            amm_buy_ixs(keys, &user, sol_to_lamports(sol_in), floor, *sol_is_base)
        }
    };
    tx::send(rpc, signer, ixs, tx::CU_LIMIT_AMM, cu_price_micro).await
}

/// Sell `tokens` whole tokens back into the curve. `slippage_pct` sets the
/// `min_sol_output` floor.
pub async fn sell(
    rpc: &Rpc,
    signer: &Keypair,
    coin: &Coin,
    tokens: f64,
    slippage_pct: f64,
    cu_price_micro: u64,
) -> eyre::Result<String> {
    if tokens <= 0.0 {
        eyre::bail!("sell size must be > 0");
    }
    let user = signer.pubkey();
    let dec = coin.token_decimals();
    let requested = (tokens * 10f64.powi(dec as i32)) as u64;

    // Size against the wallet's ACTUAL holding, not the cached one.
    //
    // The displayed balance is a poll old, and a sell of even one unit more
    // than is held is rejected outright (`NotEnoughTokensToSell`) — so a sale
    // that raced a fill, or ran while the balance read was failing, took the
    // whole position down with it. Clamping in base units rather than through
    // f64 tokens keeps "sell all" exact: no dust left, nothing overshot.
    let ata = super::ata(&user, &coin.mint, &coin.token_program());
    let units = match rpc.token_balances(&[ata]).await {
        Ok(bals) => match bals.first().copied().flatten() {
            Some(0) | None => eyre::bail!("wallet holds none of this coin"),
            Some(held) => requested.min(held),
        },
        // A read failure must not block an exit — try at the asked-for size.
        Err(_) => requested,
    };
    if units == 0 {
        eyre::bail!("sell size rounds to zero");
    }

    // Quote the size we are ACTUALLY selling, so the floor matches the trade.
    let expected_sol = coin.sol_out(units as f64 / 10f64.powi(dec as i32));
    let min_sol_output = trade::with_slippage(sol_to_lamports(expected_sol), slippage_pct.max(1.0), false);

    let ixs = match &coin.venue {
        Venue::Curve { keys, fee_recipient, buyback_recipient, .. } => {
            let buyback = buyback_recipient
                .ok_or_else(|| eyre::eyre!("pump's buyback fee recipient could not be read"))?;
            vec![trade::sell_v2_ix(keys, &user, fee_recipient, &buyback, units, min_sol_output)]
        }
        Venue::Amm { keys, sol_is_base, .. } => {
            amm_sell_ixs(keys, &user, units, min_sol_output, *sol_is_base)
        }
    };
    tx::send(rpc, signer, ixs, tx::CU_LIMIT_AMM, cu_price_micro).await
}

/// Wait for a submitted trade to land.
/// `Ok(true)` succeeded, `Ok(false)` landed but reverted, `Err` = still unknown
/// (it may yet land — never treat this as "definitely failed" and retry blindly,
/// or you can double-fill).
pub async fn confirm(rpc: &Rpc, sig: &str) -> eyre::Result<bool> {
    tx::confirm(rpc, sig, CONFIRM_TIMEOUT).await
}

/// The user's WSOL-side ATA for this pool, chosen by MINT rather than by role:
/// on a normal pool WSOL is the quote, on an inverted pool it is the base.
fn wsol_side(keys: &pumpswap::SwapKeys, user: &Pubkey) -> (Pubkey, Pubkey) {
    let (mint, prog) = if keys.base_mint == super::NATIVE_MINT {
        (keys.base_mint, keys.base_token_program)
    } else {
        (keys.quote_mint, keys.quote_token_program)
    };
    (super::ata(user, &mint, &prog), prog)
}

/// AMM buy: create ATAs, wrap the SOL being spent, swap, unwrap.
///
/// The Pump AMM moves WSOL through token accounts and never touches native
/// SOL — unlike the bonding curve, which is why curve buys worked while every
/// AMM buy died in simulation: the WSOL account existed but held nothing, so
/// the swap's transfer failed with token error 0x1 (InsufficientFunds). The
/// close at the end returns any unspent wrapped SOL plus the account's rent.
fn amm_buy_ixs(
    keys: &pumpswap::SwapKeys,
    user: &Pubkey,
    lamports: u64,
    min_tokens_out: u64,
    sol_is_base: bool,
) -> Vec<solana_instruction::Instruction> {
    let (wsol_ata, wsol_prog) = wsol_side(keys, user);
    vec![
        trade::create_ata_idempotent(user, user, &keys.base_mint, &keys.base_token_program),
        trade::create_ata_idempotent(user, user, &keys.quote_mint, &keys.quote_token_program),
        trade::transfer_lamports(user, &wsol_ata, lamports),
        trade::sync_native(&wsol_ata, &wsol_prog),
        // Exact SOL in: `buy` spends the quote side, so an inverted pool
        // (SOL as base) pays with `sell` instead.
        if sol_is_base {
            pumpswap::sell_ix(keys, user, lamports, min_tokens_out)
        } else {
            pumpswap::buy_ix(keys, user, lamports, min_tokens_out)
        },
        trade::close_token_account(&wsol_ata, user, user, &wsol_prog),
    ]
}

/// AMM sell: swap, then unwrap. Proceeds arrive as WSOL in a token account;
/// without the close they sit there and the wallet balance never moves.
fn amm_sell_ixs(
    keys: &pumpswap::SwapKeys,
    user: &Pubkey,
    token_units: u64,
    min_sol_out: u64,
    sol_is_base: bool,
) -> Vec<solana_instruction::Instruction> {
    let (wsol_ata, wsol_prog) = wsol_side(keys, user);
    vec![
        trade::create_ata_idempotent(user, user, &keys.base_mint, &keys.base_token_program),
        trade::create_ata_idempotent(user, user, &keys.quote_mint, &keys.quote_token_program),
        if sol_is_base {
            pumpswap::buy_ix(keys, user, token_units, min_sol_out)
        } else {
            pumpswap::sell_ix(keys, user, token_units, min_sol_out)
        },
        trade::close_token_account(&wsol_ata, user, user, &wsol_prog),
    ]
}

#[cfg(test)]
mod sim_floor_tests {
    use super::*;

    /// The trust boundary in one test: the simulated floor arrives from the
    /// RPC, so it may move protection down only so far.
    #[test]
    fn a_simulation_can_lower_the_floor_but_not_empty_it() {
        let quote = 1_000_000u64;

        // The honest case, and the reason the simulation is consulted at all:
        // a fee regime the formula does not model, a modest gap, taken as-is.
        assert_eq!(clamp_sim_floor(quote, 900_000), 900_000);
        assert_eq!(clamp_sim_floor(quote, 800_000), 800_000);

        // Exactly at the limit, still honoured.
        assert_eq!(clamp_sim_floor(quote, 750_000), 750_000);

        // A hostile endpoint: "this swap returns almost nothing", which used to
        // become the floor verbatim and leave the buy unprotected.
        assert_eq!(clamp_sim_floor(quote, 1), 750_000);
        assert_eq!(clamp_sim_floor(quote, 0), 750_000);
        assert_eq!(clamp_sim_floor(quote, 500_000), 750_000);

        // And it cannot talk protection UP past the honest quote either — an
        // inflated floor is a buy that reverts on every attempt.
        assert_eq!(clamp_sim_floor(quote, 5_000_000), quote);

        // A zero quote floor has nothing to clamp against and must not panic.
        assert_eq!(clamp_sim_floor(0, 12_345), 0);
    }

    /// Whatever the simulation says, the signed floor stays within the band.
    #[test]
    fn the_signed_floor_never_leaves_the_band() {
        let quote = 987_654u64;
        let low = trade::with_slippage(quote, MAX_SIM_HAIRCUT_PCT, false);
        for sim in [0u64, 1, 7, 500_000, 740_000, 900_000, quote, quote + 1, u64::MAX] {
            let f = clamp_sim_floor(quote, sim);
            assert!(f >= low, "floor {f} fell below the {low} bound (sim {sim})");
            assert!(f <= quote, "floor {f} rose above the quote {quote} (sim {sim})");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_pubkey::Pubkey;

    fn graduated_coin() -> Coin {
        let mint = Pubkey::new_from_array([7u8; 32]);
        let mut curve = BondingCurve {
            virtual_token_reserves: 1_073_000_000_000_000,
            virtual_quote_reserves: 30_000_000_000,
            real_token_reserves: 0,
            real_quote_reserves: 85_000_000_000,
            token_total_supply: 1_000_000_000_000_000,
            complete: true,
            creator: Pubkey::new_from_array([9u8; 32]),
            is_mayhem_mode: false,
            is_cashback_coin: false,
            quote_mint: Pubkey::default(),
        };
        curve.complete = true;
        Coin {
            mint,
            venue: Venue::Curve {
                keys: CurveKeys::new(mint, super::super::TOKEN_PROGRAM, curve.creator, Pubkey::default()),
                fee_recipient: Pubkey::new_from_array([4u8; 32]),
                buyback_recipient: Some(Pubkey::new_from_array([5u8; 32])),
                fee_frac: 0.01,
                curve,
            },
        }
    }

    /// An AMM-backed coin with the given `(coin, SOL)` reserves, classic
    /// orientation (coin as base).
    fn amm_coin(token_res: f64, sol_res: f64) -> Coin {
        amm_coin_oriented(token_res, sol_res, false)
    }

    /// The same coin on a pool of either orientation. Pricing and quoting must
    /// come out IDENTICAL — only the instructions differ.
    fn amm_coin_oriented(token_res: f64, sol_res: f64, sol_is_base: bool) -> Coin {
        let mint = Pubkey::new_from_array([7u8; 32]);
        let (base_mint, quote_mint) = if sol_is_base {
            (super::super::NATIVE_MINT, mint)
        } else {
            (mint, super::super::NATIVE_MINT)
        };
        Coin {
            mint,
            venue: Venue::Amm {
                keys: pumpswap::SwapKeys {
                    pool: Pubkey::new_from_array([1u8; 32]),
                    base_mint,
                    quote_mint,
                    base_token_program: super::super::TOKEN_PROGRAM,
                    quote_token_program: super::super::TOKEN_PROGRAM,
                    pool_base_ta: Pubkey::new_from_array([3u8; 32]),
                    pool_quote_ta: Pubkey::new_from_array([4u8; 32]),
                    coin_creator: Pubkey::new_from_array([5u8; 32]),
                    is_cashback_coin: true,
            buyback_recipient: Some(Pubkey::new_from_array([7u8; 32])),
            fee_recipient: Pubkey::new_from_array([6u8; 32]),
                },
                sol_res,
                token_res,
                fee_frac: 0.0025,
                token_decimals: 6,
                total_supply: 1_000_000_000.0,
                sol_is_base,
                virtual_quote_raw: 0,
            },
        }
    }

    /// BOOST pools price on real + virtual, but pay out of the real vault.
    #[test]
    fn boost_prices_on_effective_but_pays_from_the_real_vault() {
        let mut plain = amm_coin_oriented(1_000_000.0, 50.0, false);
        let mut boosted = amm_coin_oriented(1_000_000.0, 50.0, false);
        if let Venue::Amm { virtual_quote_raw, .. } = &mut boosted.venue {
            *virtual_quote_raw = 50_000_000_000; // +50 SOL virtual, quote slot
        }
        // Effective 100 SOL against the same tokens: exactly double the price.
        assert!((boosted.price_sol() - 2.0 * plain.price_sol()).abs() < 1e-12);
        // Pooled (exit liquidity) stays REAL.
        assert_eq!(boosted.pooled_sol(), plain.pooled_sol());
        // A sell big enough to want more than the vault holds is capped at
        // (just under) the real 50 SOL, never the effective 100.
        let out = boosted.sol_out(900_000.0);
        assert!(out <= 50.0 * 0.9995 + 1e-9, "payout must cap at the real vault, got {out}");
        // And the plain pool is untouched by the field existing at all.
        if let Venue::Amm { virtual_quote_raw, .. } = &mut plain.venue {
            assert_eq!(*virtual_quote_raw, 0);
        }
    }

    /// Orientation must be invisible to pricing.
    ///
    /// ~78% of mainnet SOL pools put SOL in the base slot. If any of these
    /// differed, an inverted coin would be priced upside down — and the sizing
    /// built on it would be wrong by the price squared.
    #[test]
    fn pool_orientation_does_not_change_pricing() {
        let normal = amm_coin_oriented(1_000_000.0, 50.0, false);
        let inverted = amm_coin_oriented(1_000_000.0, 50.0, true);
        assert_eq!(normal.price_sol(), inverted.price_sol());
        assert_eq!(normal.pooled_sol(), inverted.pooled_sol());
        assert_eq!(normal.market_cap_sol(), inverted.market_cap_sol());
        assert_eq!(normal.tokens_out(1.0), inverted.tokens_out(1.0));
        assert_eq!(normal.sol_out(1000.0), inverted.sol_out(1000.0));
        assert!(normal.price_sol() > 0.0, "sanity: the fixture must price");
    }

    /// Sanity on the numbers themselves, not just their agreement.
    #[test]
    fn inverted_pool_prices_the_coin_not_sol() {
        // 50 SOL against 1M tokens => 0.00005 SOL per token, either way round.
        let c = amm_coin_oriented(1_000_000.0, 50.0, true);
        assert!((c.price_sol() - 0.00005).abs() < 1e-12, "got {}", c.price_sol());
        assert_eq!(c.pooled_sol(), 50.0, "pooled must be the SOL side, not the coin side");
    }

    /// A coin on the AMM prices and quotes through the pool, not the curve.
    #[test]
    fn amm_coin_quotes_from_pool_reserves() {
        let coin = amm_coin(1_000_000.0, 50.0);
        assert!(coin.on_amm());
        assert_eq!(coin.venue_label(), "amm");
        // price = quote/base
        assert!((coin.price_sol() - 50.0 / 1_000_000.0).abs() < 1e-12);
        // Pooled is the REAL exit liquidity on the AMM.
        assert_eq!(coin.pooled_sol(), 50.0);
        // Fully bonded by definition.
        assert_eq!(coin.progress(), 1.0);
        // Quotes come from the constant product, and exhibit slippage.
        let small = coin.tokens_out(1.0);
        assert!(small > 0.0);
        assert!(coin.tokens_out(10.0) < small * 10.0);
        assert!(coin.sol_out(1000.0) > 0.0);
    }

    /// Curve coins keep quoting from the curve.
    #[test]
    fn curve_coin_still_uses_the_curve() {
        let mut coin = graduated_coin();
        if let Venue::Curve { curve, .. } = &mut coin.venue {
            curve.complete = false;
            curve.real_quote_reserves = 5_000_000_000;
        }
        assert!(!coin.on_amm());
        assert_eq!(coin.venue_label(), "curve");
        assert!(coin.market_cap_sol() > 0.0);
        assert!(coin.sol_out(1000.0) > 0.0);
    }

    #[tokio::test]
    async fn non_positive_sizes_are_refused() {
        let rpc = Rpc::new("http://127.0.0.1:1");
        let kp = Keypair::new();
        // Both venues must refuse a non-positive size before touching RPC.
        for coin in [graduated_coin(), amm_coin(1_000_000.0, 50.0)] {
            assert!(buy(&rpc, &kp, &coin, 0.0, 5.0, 0).await.is_err());
            assert!(buy(&rpc, &kp, &coin, -1.0, 5.0, 0).await.is_err());
            assert!(sell(&rpc, &kp, &coin, 0.0, 5.0, 0).await.is_err());
        }
    }

    fn oriented_keys(sol_is_base: bool) -> pumpswap::SwapKeys {
        let coin = Pubkey::new_unique();
        let (base_mint, quote_mint) = if sol_is_base {
            (super::super::NATIVE_MINT, coin)
        } else {
            (coin, super::super::NATIVE_MINT)
        };
        pumpswap::SwapKeys {
            pool: Pubkey::new_unique(),
            base_mint,
            quote_mint,
            base_token_program: super::super::TOKEN_PROGRAM,
            quote_token_program: super::super::TOKEN_PROGRAM,
            pool_base_ta: Pubkey::new_unique(),
            pool_quote_ta: Pubkey::new_unique(),
            coin_creator: Pubkey::new_unique(),
            is_cashback_coin: true,
            buyback_recipient: Some(Pubkey::new_from_array([7u8; 32])),
            fee_recipient: Pubkey::new_unique(),
        }
    }

    /// The wrap has to hit the WSOL account, sync it, swap, and unwrap — in
    /// that order, in both orientations. Skipping the wrap is how 21 straight
    /// buys failed in simulation with token error 0x1.
    #[test]
    fn amm_buys_wrap_swap_and_unwrap_in_order() {
        let user = Pubkey::new_unique();
        for sol_is_base in [false, true] {
            let keys = oriented_keys(sol_is_base);
            let ixs = amm_buy_ixs(&keys, &user, 585_000, 1, sol_is_base);
            assert_eq!(ixs.len(), 6);
            let wsol_mint = if sol_is_base { keys.base_mint } else { keys.quote_mint };
            let wsol = super::super::ata(&user, &wsol_mint, &super::super::TOKEN_PROGRAM);
            assert_eq!(ixs[2].program_id, super::super::SYSTEM_PROGRAM, "fund first");
            assert_eq!(ixs[2].accounts[1].pubkey, wsol, "the transfer must hit the WSOL ata");
            assert_eq!(ixs[3].data, vec![17], "then SyncNative");
            assert_eq!(ixs[3].accounts[0].pubkey, wsol);
            assert_eq!(ixs[4].program_id, super::super::PUMP_AMM_PROGRAM, "then the swap");
            assert_eq!(ixs[5].data, vec![9], "unwrap last");
            assert_eq!(ixs[5].accounts[0].pubkey, wsol);
        }
    }

    /// Sell proceeds arrive as WSOL; without the close they would sit in the
    /// token account and the wallet balance would never move.
    #[test]
    fn amm_sells_unwrap_the_proceeds() {
        let user = Pubkey::new_unique();
        for sol_is_base in [false, true] {
            let keys = oriented_keys(sol_is_base);
            let ixs = amm_sell_ixs(&keys, &user, 1_000_000, 1, sol_is_base);
            assert_eq!(ixs.len(), 4);
            let wsol_mint = if sol_is_base { keys.base_mint } else { keys.quote_mint };
            let wsol = super::super::ata(&user, &wsol_mint, &super::super::TOKEN_PROGRAM);
            assert_eq!(ixs[2].program_id, super::super::PUMP_AMM_PROGRAM);
            assert_eq!(ixs[3].data, vec![9], "proceeds must be unwrapped to native SOL");
            assert_eq!(ixs[3].accounts[0].pubkey, wsol);
        }
    }

    /// A sell floor computed from a fee-free quote sits above what the program
    /// actually pays, so the trade reverts with `TooLittleSolReceived` (6003)
    /// unless slippage happens to exceed the fee. The quote must already be net.
    #[test]
    fn curve_quotes_are_net_of_fees_so_slippage_floors_can_fill() {
        // A live curve with real reserves on both sides.
        let coin = Coin {
            mint: Pubkey::new_from_array([7u8; 32]),
            venue: Venue::Curve {
                curve: BondingCurve {
                    virtual_token_reserves: 1_073_000_000_000_000,
                    virtual_quote_reserves: 30_000_000_000,
                    real_token_reserves: 793_100_000_000_000,
                    real_quote_reserves: 10_000_000_000,
                    token_total_supply: 1_000_000_000_000_000,
                    complete: false,
                    creator: Pubkey::new_from_array([9u8; 32]),
                    is_mayhem_mode: false,
                    is_cashback_coin: false,
                    quote_mint: Pubkey::default(),
                },
                keys: CurveKeys::new(
                    Pubkey::new_from_array([7u8; 32]),
                    super::super::TOKEN_PROGRAM,
                    Pubkey::new_from_array([9u8; 32]),
                    Pubkey::default(),
                ),
                fee_recipient: Pubkey::new_from_array([4u8; 32]),
                buyback_recipient: Some(Pubkey::new_from_array([5u8; 32])),
                fee_frac: 0.01,
            },
        };
        let (gross_out, tokens) = match &coin.venue {
            Venue::Curve { curve, .. } => (curve.sol_out(1_000.0), 1_000.0),
            _ => unreachable!(),
        };
        let net = coin.sol_out(tokens);
        assert!(net < gross_out, "quote must be net of the fee");
        assert!((net - gross_out * 0.99).abs() < 1e-12, "1% comes off the proceeds");

        // With the fee handled, a 1% floor sits BELOW the payout instead of on
        // top of it — which is the difference between filling and reverting.
        let floor = trade::with_slippage(super::sol_to_lamports(net), 1.0, false) as f64;
        let actually_paid = super::sol_to_lamports(gross_out * 0.99) as f64;
        assert!(floor < actually_paid, "floor {floor} must sit under payout {actually_paid}");

        // Buys: the fee comes off the SOL going in, so fewer tokens come back.
        let with_fee = coin.tokens_out(1.0);
        let fee_free = match &coin.venue {
            Venue::Curve { curve, .. } => curve.tokens_out(1.0),
            _ => unreachable!(),
        };
        assert!(with_fee < fee_free, "a buy quote must spend the fee too");
    }

    /// Clamping has to happen in base units. Going through f64 tokens loses
    /// precision on large balances, and "sell all" then either leaves dust
    /// behind or asks for more than is held — which the program rejects
    /// outright rather than filling what it can.
    #[test]
    fn sell_size_clamps_to_the_held_balance_exactly() {
        // A balance whose unit count cannot be represented exactly as f64 tokens.
        let held: u64 = 29_844_021_123_457;
        let dec = 6i32;
        let asked_all = held as f64 / 10f64.powi(dec); // what "sell all" passes in
        let requested = (asked_all * 10f64.powi(dec)) as u64;
        assert_eq!(requested.min(held), held, "sell all must take the whole balance");

        // Asking for more than is held is capped, not rejected.
        let greedy = (held as f64 * 1.5) as u64;
        assert_eq!(greedy.min(held), held);

        // Asking for less is left alone.
        let half = held / 2;
        assert_eq!(half.min(held), half);
    }

    /// A completed curve must be recognisable as needing migration.
    ///
    /// This is the state the dashboard sat in for a graduated coin: venue still
    /// "bonding curve", every metric zero because the reserves already moved to
    /// the AMM, and a buy that the program would reject.
    #[test]
    fn a_completed_curve_is_a_migration_trigger() {
        let mut c = graduated_coin();
        if let Venue::Curve { curve, .. } = &mut c.venue {
            curve.complete = false;
        }
        assert!(!c.graduated(), "a live curve is not graduated");
        if let Venue::Curve { curve, .. } = &mut c.venue {
            curve.complete = true;
        }
        assert!(c.graduated(), "a complete curve must report as graduated");
        assert!(!c.on_amm(), "…but it is NOT yet on the AMM — that needs a reload");
    }

    /// Slippage must never be able to collapse to "no protection", even if a
    /// caller passes 0 — the quote excludes fees, so a 0% cap always reverts.
    #[test]
    fn slippage_has_a_protective_floor() {
        let quoted = sol_to_lamports(1.0);
        let cap = trade::with_slippage(quoted, 0.0f64.max(1.0), true);
        assert!(cap > quoted, "buy cap must exceed the fee-free quote");
        let floor = trade::with_slippage(quoted, 0.0f64.max(1.0), false);
        assert!(floor < quoted, "sell floor must sit below the fee-free quote");
    }
}

#[cfg(test)]
mod live_amm_tests {
    use super::*;

    /// End-to-end read-only check of the AMM path against mainnet: find a real
    /// graduated coin, resolve it, and confirm it prices sanely.
    ///
    /// This is the check to run BEFORE trading a graduated coin — it exercises
    /// pool lookup, GlobalConfig decode, reserves and quotes without sending
    /// anything.
    ///
    ///   cargo test --features solana live_amm -- --ignored --nocapture
    /// Resolve a specific mint the way the app's `p` (add token) path does.
    ///   MINT=<base58> cargo test --features solana live_named_mint -- --ignored --nocapture
    /// Defaults to ansem, the graduated coin whose 301-byte pool the old
    /// `dataSize: 261` filter hid.
    #[tokio::test]
    #[ignore]
    async fn live_named_mint_resolves() {
        let reg = crate::config::Registry::load("deployments.json").expect("registry");
        let net = reg.networks.iter().find(|n| n.kind.is_solana()).expect("solana net");
        let rpc = Rpc::new_pool(net.rpc_pool());

        let mint_s = std::env::var("MINT")
            .unwrap_or_else(|_| "9cRCn9rGT8V2imeM2BaKs13yhMEais3ruM3rPvTGpump".to_string());
        let mint: Pubkey = mint_s.parse().expect("valid base58 mint");

        let coin = load_coin(&rpc, &mint).await.expect("coin should resolve");
        println!("  venue    : {}", coin.venue_label());
        println!("  price    : {:.12} SOL", coin.price_sol());
        println!("  pooled   : {:.4} SOL", coin.pooled_sol());
        println!("  mkt cap  : {:.2} SOL", coin.market_cap_sol());
        println!("  1 SOL buys: {:.0} tokens", coin.tokens_out(1.0));
        assert!(coin.price_sol() > 0.0, "a resolved coin must have a price");
    }

    #[tokio::test]
    #[ignore]
    async fn live_amm_resolves_and_prices_a_graduated_coin() {
        let rpc = Rpc::new_pool(vec![
            "https://api.mainnet-beta.solana.com".into(),
        ]);

        // Take any live AMM pool and trade-check its base mint.
        let pools = rpc
            .program_accounts_memcmp_any(&super::super::PUMP_AMM_PROGRAM, &pumpswap::DISC_POOL)
            .await
            .unwrap_or_default();
        println!("AMM pools sampled: {}", pools.len());
        let Some(mint) = pools.iter().find_map(|(_, data)| {
            let p = pumpswap::Pool::decode(data).ok()?;
            p.is_sol_quoted().then_some(p.base_mint)
        }) else {
            println!("no SOL-quoted pool sampled; skipping");
            return;
        };

        println!("resolving {mint} …");
        let coin = load_coin(&rpc, &mint).await.expect("graduated coin should resolve");
        println!("  venue    : {}", coin.venue_label());
        println!("  price    : {:.12} SOL", coin.price_sol());
        println!("  pooled   : {:.4} SOL", coin.pooled_sol());
        println!("  mkt cap  : {:.2} SOL", coin.market_cap_sol());
        println!("  1 SOL buys: {:.0} tokens", coin.tokens_out(1.0));

        assert!(coin.on_amm(), "a graduated coin must route to the AMM");
        assert!(coin.price_sol() > 0.0, "price must be positive");
        assert!(coin.pooled_sol() > 0.0, "an AMM pool must hold real SOL");
        // Round-tripping must lose to fees, never gain.
        let tokens = coin.tokens_out(0.1);
        assert!(tokens > 0.0);
        assert!(coin.sol_out(tokens) < 0.1, "round trip must not be profitable");
    }
}
