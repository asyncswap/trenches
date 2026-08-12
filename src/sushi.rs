// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! SushiSwap swap calldata, for pools.fun launches and anything else living in
//! a Sushi V3 pool.
//!
//! A v3 router is bound to its factory, so Uniswap's SwapRouter02 cannot touch
//! these pools at all — they need Sushi's own path. That path is two layers:
//! RedSnwapper takes the input and enforces the minimum on what actually
//! reaches the trader, and the RouteProcessor executes a packed byte "route"
//! describing the hops. Everything here builds one V3 hop, which is all a
//! pools.fun token ever needs.
//!
//! Two details in here are not guesses — they were bisected against Sushi's own
//! live API calldata on a fork, and getting either wrong costs money rather
//! than reverting:
//!
//! 1. `takeSurplus` MUST be false. With it true the RouteProcessor forwards
//!    only `amountOutMin` to the recipient and keeps everything above it.
//!    Sushi's API sets it true and compensates with a tightly quoted minimum;
//!    a bot that passed a loose minimum would hand over nearly the whole fill.
//! 2. The final leg pays the TRADER directly, not the RouteProcessor. That is
//!    what makes (1) safe without needing a quote at all: the processor never
//!    holds the output, so there is no surplus to keep.

use alloy::primitives::{address, Address, Bytes, U256};
use alloy::sol_types::SolCall;

use crate::contracts::*;

/// Sushi's stand-in for native ETH in router arguments. The route itself always
/// names real WETH; only the outer call uses this sentinel.
pub const NATIVE: Address = address!("EeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE");

/// "Use the whole balance for this leg" — the route's per-pool share is a
/// uint16 fraction, and 65535 means all of it.
const FULL_SHARE: u16 = 65_535;

/// One V3 hop, in the RouteProcessor's packed command language.
///
/// `token_in` is the REAL token entering the pool (WETH, never the sentinel).
/// `wrap_native` prepends an ETH->WETH leg, `unwrap_native` appends WETH->ETH.
/// When an unwrap follows, the pool must pay the processor so it has WETH to
/// unwrap; otherwise the pool pays `recipient` and we are done in one hop.
fn v3_route(
    token_in: Address,
    pool: Address,
    zero_for_one: bool,
    wrap_native: bool,
    unwrap_native: bool,
    recipient: Address,
) -> Vec<u8> {
    let rp = sushi_route_processor();
    let mut r = Vec::with_capacity(160);

    // Header: version, an unused uint40, and the command count.
    r.push(1u8);
    r.extend_from_slice(&[0u8; 5]);
    r.extend_from_slice(&2u16.to_be_bytes());

    if wrap_native {
        // processNative -> one leg -> whole balance -> wrapNative(direction 1).
        r.push(3);
        r.push(1);
        r.extend_from_slice(&FULL_SHARE.to_be_bytes());
        r.push(2);
        r.push(1);
        r.extend_from_slice(rp.as_slice());
        r.extend_from_slice(token_in.as_slice());
    }

    // processMyERC20 -> one leg -> whole balance -> univ3 pool.
    let pool_recipient = if unwrap_native { rp } else { recipient };
    r.push(1);
    r.extend_from_slice(token_in.as_slice());
    r.push(1);
    r.extend_from_slice(&FULL_SHARE.to_be_bytes());
    r.push(1);
    r.extend_from_slice(pool.as_slice());
    r.push(u8::from(zero_for_one));
    r.extend_from_slice(pool_recipient.as_slice());
    r.push(0);
    r.extend_from_slice(&[0u8; 6]);

    if unwrap_native {
        // processMyERC20(WETH) -> wrapNative(direction 0) -> straight to us.
        r.push(1);
        r.extend_from_slice(weth().as_slice());
        r.push(1);
        r.extend_from_slice(&FULL_SHARE.to_be_bytes());
        r.push(2);
        r.push(0);
        r.extend_from_slice(recipient.as_slice());
    }

    r
}

/// Wrap a route in the RouteProcessor call and then in RedSnwapper's.
fn snwap_calldata(
    token_in: Address,
    token_out: Address,
    amount_in: u128,
    min_out: u128,
    recipient: Address,
    route: Vec<u8>,
) -> Bytes {
    let executor_data = ISushiRouteProcessor::processRouteCall {
        tokenIn: token_in,
        amountIn: U256::from(amount_in),
        tokenOut: token_out,
        amountOutMin: U256::from(min_out),
        to: recipient,
        route: route.into(),
        // See the module note: true would skim everything above the minimum.
        takeSurplus: false,
        referralCode: 0,
    }
    .abi_encode();

    IRedSnwapper::snwapCall {
        tokenIn: token_in,
        amountIn: U256::from(amount_in),
        recipient,
        tokenOut: token_out,
        amountOutMin: U256::from(min_out),
        executor: sushi_route_processor(),
        executorData: executor_data.into(),
    }
    .abi_encode()
    .into()
}

/// True when a pool's quote side is WETH, so the trade can be denominated in
/// native ETH and wrapped/unwrapped inside the route.
fn is_native_quote(quote: Address) -> bool {
    quote == weth()
}

/// BUY: spend the quote asset, receive the launch token.
///
/// For a WETH-quoted pool the caller sends native ETH as value and the route
/// wraps it. For a USDG-quoted pool the input is a plain ERC-20 pull, so the
/// token must already be approved to `sushi_red_snwapper()` and no value is
/// sent — see [`buy_value`].
pub fn sushi_buy_calldata(
    token: Address,
    pool: Address,
    quote: Address,
    amount_in: u128,
    min_out: u128,
    trader: Address,
) -> Bytes {
    let native = is_native_quote(quote);
    // The pool sorts by real addresses; the sentinel never enters this.
    let zero_for_one = quote < token;
    let route = v3_route(quote, pool, zero_for_one, native, false, trader);
    let token_in = if native { NATIVE } else { quote };
    snwap_calldata(token_in, token, amount_in, min_out, trader, route)
}

/// SELL: spend the launch token, receive the quote asset (unwrapped to native
/// ETH when the pool is WETH-quoted). The token must be approved to
/// `sushi_red_snwapper()` first — the RouteProcessor is NOT the spender.
pub fn sushi_sell_calldata(
    token: Address,
    pool: Address,
    quote: Address,
    amount_in: u128,
    min_out: u128,
    trader: Address,
) -> Bytes {
    let native = is_native_quote(quote);
    let zero_for_one = token < quote;
    let route = v3_route(token, pool, zero_for_one, false, native, trader);
    let token_out = if native { NATIVE } else { quote };
    snwap_calldata(token, token_out, amount_in, min_out, trader, route)
}

/// The ETH to send with a buy: the input amount for a WETH-quoted pool, nothing
/// for an ERC-20-quoted one (where sending value would strand it).
pub fn buy_value(quote: Address, amount_in: u128) -> U256 {
    if is_native_quote(quote) {
        U256::from(amount_in)
    } else {
        U256::ZERO
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real pools.fun launch and its pool, plus the trader used when these
    // fixtures were captured from a passing fork test against the live
    // RouteProcessor. See scratchpad SUSHI_SPEC.md / sushiroute tests.
    const TOKEN: Address = address!("08381AB31D2B3E70D47F8256d71C56a9e14A13d1");
    const POOL: Address = address!("619aa83b66Bd9738eB665FFc993B64048837F9FF");
    const TRADER: Address = address!("10e951fa67b511d044803c7757da445ddf646f6d");

    /// Byte-for-byte against the route that actually executed on chain.
    #[test]
    fn buy_route_matches_the_verified_bytes() {
        let route = v3_route(weth(), POOL, false, true, false, TRADER);
        assert_eq!(
            alloy::hex::encode(&route),
            "01000000000000020301ffff02010e867974275cd31c25015c2753c9d75f9f355379\
             0bd7d308f8e1639fab988df18a8011f41eacad73010bd7d308f8e1639fab988df18a\
             8011f41eacad7301ffff01619aa83b66bd9738eb665ffc993b64048837f9ff0010e9\
             51fa67b511d044803c7757da445ddf646f6d00000000000000"
                .replace(['\n', ' '], "")
        );
    }

    #[test]
    fn sell_route_matches_the_verified_bytes() {
        let route = v3_route(TOKEN, POOL, true, false, true, TRADER);
        assert_eq!(
            alloy::hex::encode(&route),
            "01000000000000020108381ab31d2b3e70d47f8256d71c56a9e14a13d101ffff0161\
             9aa83b66bd9738eb665ffc993b64048837f9ff010e867974275cd31c25015c2753c9\
             d75f9f35537900000000000000010bd7d308f8e1639fab988df18a8011f41eacad73\
             01ffff020010e951fa67b511d044803c7757da445ddf646f6d"
                .replace(['\n', ' '], "")
        );
    }

    /// The launch token is always token0, so a buy pays token1 for token0.
    #[test]
    fn direction_follows_address_order() {
        assert!(TOKEN < weth(), "a pools.fun token sorts below its quote");
        let buy = v3_route(weth(), POOL, weth() < TOKEN, true, false, TRADER);
        let sell = v3_route(TOKEN, POOL, TOKEN < weth(), false, true, TRADER);
        // The direction byte sits immediately after the pool address.
        let dir = |r: &[u8]| {
            let at = r.windows(20).position(|w| w == POOL.as_slice()).unwrap();
            r[at + 20]
        };
        assert_eq!(dir(&buy), 0, "buying token0 is not zeroForOne");
        assert_eq!(dir(&sell), 1, "selling token0 is zeroForOne");
    }

    /// A buy sends ETH only when the pool is WETH-quoted; a USDG pool is an
    /// ERC-20 pull and value would be stranded.
    #[test]
    fn only_weth_quoted_buys_send_value() {
        assert_eq!(buy_value(weth(), 1_000), U256::from(1_000));
        assert_eq!(buy_value(usdg(), 1_000), U256::ZERO);
    }

    /// A USDG-quoted pool must never wrap: no native legs in either direction.
    #[test]
    fn erc20_quoted_pools_have_no_wrap_legs() {
        let buy = sushi_buy_calldata(TOKEN, POOL, usdg(), 1_000, 1, TRADER);
        // The wrap leg is the only place the sentinel-free WETH address would
        // appear for a USDG pool.
        assert!(
            !buy.windows(20).any(|w| w == weth().as_slice()),
            "a USDG-quoted buy must not touch WETH"
        );
    }

    /// takeSurplus must stay false, or the processor keeps everything above the
    /// minimum. Encoded as the second-to-last word of the executor calldata.
    #[test]
    fn take_surplus_is_never_set() {
        let data = sushi_buy_calldata(TOKEN, POOL, weth(), 1_000, 1, TRADER);
        let decoded = IRedSnwapper::snwapCall::abi_decode(&data, true).unwrap();
        let inner = ISushiRouteProcessor::processRouteCall::abi_decode(&decoded.executorData, true).unwrap();
        assert!(!inner.takeSurplus, "takeSurplus would skim the fill");
        assert_eq!(inner.to, TRADER, "output must be paid to the trader");
    }

    /// A plain Uniswap v3 pool must not be announced as a launchpad it has
    /// nothing to do with, and a Sushi pool must not be announced as Uniswap —
    /// the banner is how a trader checks they are on the venue they meant.
    #[test]
    fn venue_names_do_not_claim_the_wrong_launchpad() {
        use crate::engine::PoolKind;
        let uni = PoolKind::V3 { pool_addr: POOL, weth_is_token0: false };
        let sushi = PoolKind::SushiV3 { pool_addr: POOL, quote: weth() };
        assert_eq!(uni.banner_name(), "UNISWAP V3", "a v3 pool is not automatically a Pons graduation");
        assert_eq!(sushi.banner_name(), "POOLS.FUN");
        assert_eq!(sushi.venue_label(), "SushiSwap V3");
        assert_ne!(sushi.venue_short(), uni.venue_short(), "the two venues must be distinguishable");
    }
}
