// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! Uniswap v4 calldata: swaps via Universal Router, add/close liquidity via
//! PositionManager. Built with alloy ABI encoding (the sol! structs), so the
//! encoding is correct by construction — no hand-templated bytes.

use alloy::primitives::{Address, Bytes, U256};
use alloy::sol_types::{SolCall, SolValue};

use crate::contracts::*;

pub const FAR_DEADLINE: u64 = 4_102_444_800; // year 2100

/// Build UniversalRouter.execute() calldata for a single-hop v4 exact-in swap
/// on an ETH-paired pool. buy = ETH->token (send `amount_in` as value).
pub fn swap_calldata(
    token: Address,
    fee: u32,
    tick_spacing: i32,
    buy: bool,
    amount_in: u128,
    min_out: u128,
) -> Bytes {
    let key = PoolKey {
        currency0: Address::ZERO,
        currency1: token,
        fee: fee.try_into().unwrap(),
        tickSpacing: tick_spacing.try_into().unwrap(),
        hooks: Address::ZERO,
    };
    let (input_cur, output_cur) = if buy {
        (Address::ZERO, token)
    } else {
        (token, Address::ZERO)
    };

    let params0 = ExactInputSingleParams {
        poolKey: key.clone(),
        zeroForOne: buy,
        amountIn: amount_in,
        amountOutMinimum: min_out,
        minHopPriceX36: U256::ZERO,
        hookData: Bytes::new(),
    }
    .abi_encode();

    // SETTLE_ALL(inputCurrency, amountIn), TAKE_ALL(outputCurrency, minOut).
    let params1 = (input_cur, U256::from(amount_in)).abi_encode_params();
    let params2 = (output_cur, U256::from(min_out)).abi_encode_params();

    let actions = Bytes::from(vec![SWAP_EXACT_IN_SINGLE, SETTLE_ALL, TAKE_ALL]);
    let params: Vec<Bytes> = vec![params0.into(), params1.into(), params2.into()];
    let v4_input = (actions, params).abi_encode_params();

    let commands = Bytes::from(vec![V4_SWAP]);
    let inputs: Vec<Bytes> = vec![v4_input.into()];

    IUniversalRouter::executeCall {
        commands,
        inputs,
        deadline: U256::from(FAR_DEADLINE),
    }
    .abi_encode()
    .into()
}

/// Build PositionManager.modifyLiquidities() calldata to open a position.
#[allow(clippy::too_many_arguments)] // a swap needs every one of these; bundling them into a struct would only move the list
pub fn add_liquidity_calldata(
    token: Address,
    fee: u32,
    tick_spacing: i32,
    tick_lower: i32,
    tick_upper: i32,
    liquidity: U256,
    amount0_max: u128,
    amount1_max: u128,
    owner: Address,
) -> Bytes {
    let key = PoolKey {
        currency0: Address::ZERO,
        currency1: token,
        fee: fee.try_into().unwrap(),
        tickSpacing: tick_spacing.try_into().unwrap(),
        hooks: Address::ZERO,
    };
    // Encode the MINT params as a FLAT tuple (not the struct's abi_encode,
    // which prepends a 32-byte offset because hookData is dynamic — that shift
    // makes v4's CalldataDecoder read past the slice: SliceOutOfBounds 0x3b99b53d).
    // Matches the proven Zig layout: (PoolKey, int24, int24, uint256, uint128,
    // uint128, address, bytes).
    let tick_lower_i24: alloy::primitives::aliases::I24 = tick_lower.try_into().unwrap();
    let tick_upper_i24: alloy::primitives::aliases::I24 = tick_upper.try_into().unwrap();
    let mint = (
        key.clone(),
        tick_lower_i24,
        tick_upper_i24,
        liquidity,
        amount0_max,
        amount1_max,
        owner,
        Bytes::new(),
    )
        .abi_encode_params();
    // SETTLE_PAIR(currency0, currency1), SWEEP(currency0, owner).
    let settle = (key.currency0, key.currency1).abi_encode_params();
    let sweep = (key.currency0, owner).abi_encode_params();

    let actions = Bytes::from(vec![MINT_POSITION, SETTLE_PAIR, SWEEP]);
    let params: Vec<Bytes> = vec![mint.into(), settle.into(), sweep.into()];
    let unlock = (actions, params).abi_encode_params();

    IPositionManager::modifyLiquiditiesCall {
        unlockData: unlock.into(),
        deadline: U256::from(FAR_DEADLINE),
    }
    .abi_encode()
    .into()
}

/// Build calldata to close (burn) a position, returning both tokens.
pub fn close_liquidity_calldata(
    token_id: U256,
    token: Address,
    owner: Address,
    amount0_min: u128,
    amount1_min: u128,
) -> Bytes {
    // BURN_POSITION(tokenId, amount0Min, amount1Min, hookData),
    // TAKE_PAIR(currency0=ETH, currency1=token, recipient).
    //
    // These were both 0 — "give me whatever you like". A burn is a withdrawal
    // at the CURRENT price, so anyone who moves the pool through the position's
    // range in the seconds before it mines converts the whole position into the
    // depreciating side, and a zero minimum accepts it.
    let burn = (token_id, amount0_min, amount1_min, Bytes::new()).abi_encode_params();
    let take = (Address::ZERO, token, owner).abi_encode_params();

    let actions = Bytes::from(vec![BURN_POSITION, TAKE_PAIR]);
    let params: Vec<Bytes> = vec![burn.into(), take.into()];
    let unlock = (actions, params).abi_encode_params();

    IPositionManager::modifyLiquiditiesCall {
        unlockData: unlock.into(),
        deadline: U256::from(FAR_DEADLINE),
    }
    .abi_encode()
    .into()
}

/// Exact v4 exact-input quote within the current tick range (from mmsaki/uv4).
/// r0/r1 are the ETH/token virtual reserves (human units). `fee` is the pool
/// fee in hundredths-of-a-bip (3000 = 0.3%, 10000 = 1%).
pub fn quote_out(r0: f64, r1: f64, amount_in: f64, buying: bool, fee: u32) -> f64 {
    if r0 <= 0.0 || r1 <= 0.0 || amount_in <= 0.0 {
        return 0.0;
    }
    let l = (r0 * r1).sqrt();
    let sqrt_p = (r1 / r0).sqrt();
    let net = amount_in * (1.0 - fee as f64 / 1_000_000.0);
    if buying {
        let new_sqrt = l * sqrt_p / (l + net * sqrt_p);
        l * (sqrt_p - new_sqrt)
    } else {
        let new_sqrt = sqrt_p + net / l;
        l * (1.0 / sqrt_p - 1.0 / new_sqrt)
    }
}

#[cfg(test)]
mod flaunch_swap_tests {
    use super::*;
    use alloy::primitives::address;

    const TOKEN: Address = address!("1111111111111111111111111111111111111111");
    const AMOUNT_IN: u128 = 1_000_000_000_000_000; // 0.001 ETH
    const MIN_OUT: u128 = 12_345_678_901_234_567_890;

    /// One hop, flETH <-> coin, hooked on the Flaunch position manager.
    ///
    /// The two-hop shape this replaced came from the Flaunch SDK and encoded
    /// cleanly, but reverted on every real buy: its first leg reaches flETH
    /// through a pool on FLETH_HOOKS, and a working trade on this chain
    /// (0x3d54cbcc…) does no such hop. Byte-equality against the SDK fixture
    /// went with it — matching an encoding that does not execute is not a test.
    #[test]
    fn flaunch_calldata_decodes_back() {
        for buy in [true, false] {
            let data = flaunch_hop_calldata(TOKEN, buy, AMOUNT_IN, MIN_OUT);
            let call = IUniversalRouter::executeCall::abi_decode(&data, true).unwrap();
            assert_eq!(call.commands.as_ref(), [V4_SWAP]);
            assert_eq!(call.deadline, U256::from(FAR_DEADLINE));

            let (actions, params): (Bytes, Vec<Bytes>) =
                SolValue::abi_decode_params(&call.inputs[0], true).unwrap();
            assert_eq!(actions.as_ref(), [SWAP_EXACT_IN, SETTLE_ALL, TAKE_ALL]);
            assert_eq!(params.len(), 3);

            // The swap params blob is offset-prefixed (a dynamic tuple), the
            // layout this router's CalldataDecoder expects — not the flat
            // layout MINT needs (see add_liquidity_calldata).
            assert_eq!(U256::from_be_slice(&params[0][..32]), U256::from(0x20));
            let p: ExactInputParams = SolValue::abi_decode(&params[0], true).unwrap();
            assert_eq!(p.amountIn, AMOUNT_IN);
            assert_eq!(p.amountOutMinimum, MIN_OUT);
            assert!(p.minHopPriceX36.is_empty());
            // ONE hop. Two was the shape that reverted.
            assert_eq!(p.path.len(), 1);
            let spacing: alloy::primitives::aliases::I24 =
                FLAUNCH_TICK_SPACING.try_into().unwrap();
            for hop in &p.path {
                assert_eq!(hop.fee, alloy::primitives::aliases::U24::ZERO);
                assert_eq!(hop.tickSpacing, spacing);
            }
            // Both directions hop between flETH and the coin on the Flaunch
            // hook — never through FLETH_HOOKS, and never out of native ETH.
            assert_eq!((p.path[0].intermediateCurrency, p.path[0].hooks),
                       (if buy { TOKEN } else { FLETH }, FLAUNCH_PM));
            assert_eq!(p.currencyIn, if buy { FLETH } else { TOKEN });
            assert_ne!(p.currencyIn, Address::ZERO, "the input is never native ETH");
        }
    }
}


/// Wrap ETH into flETH: `deposit{value: amount}(0)`.
///
/// Not the WETH shape. flETH's `deposit` takes an amount of a DIFFERENT token
/// to pull in (which needs an allowance, and reverts without one); passing 0
/// means "just wrap the ETH I sent". Confirmed against the chain — `deposit()`
/// with no argument reverts, and `deposit(amount)` fails on allowance.
pub fn fleth_deposit_calldata() -> Bytes {
    IFLETH::depositCall { _amount: U256::ZERO }.abi_encode().into()
}

/// Unwrap flETH back to ETH.
pub fn fleth_withdraw_calldata(amount: u128) -> Bytes {
    IFLETH::withdrawCall { _amount: U256::from(amount) }.abi_encode().into()
}

/// A Flaunch swap as the chain actually does one: a SINGLE v4 hop between flETH
/// and the coin, with flETH obtained separately.
///
/// The two-hop version this replaces tried to reach flETH through a v4 pool on
/// FLETH_HOOKS as the first leg, and that leg is what failed — the Flaunch hook
/// then reverted with HookCallFailed, or the coin's transfer failed, depending
/// on where it gave up. A real working buy on this chain (tx 0x3d54cbcc…) does
/// no such hop: it obtains flETH by other means and then makes ONE v4 swap,
/// flETH -> coin, fee 0, spacing 60, hooks = the Flaunch position manager.
///
/// The router pulls `currencyIn` from the caller through Permit2, so flETH
/// needs the same approval pair the sell side already arranges for the coin.
pub fn flaunch_hop_calldata(token: Address, buy: bool, amount_in: u128, min_out: u128) -> Bytes {
    let (input_cur, output_cur, hop_cur) =
        if buy { (FLETH, token, token) } else { (token, FLETH, FLETH) };
    let path = vec![PathKey {
        intermediateCurrency: hop_cur,
        // Flaunch charges its cut in the hook; the pool fee really is 0.
        fee: alloy::primitives::aliases::U24::ZERO,
        tickSpacing: FLAUNCH_TICK_SPACING.try_into().unwrap(),
        hooks: FLAUNCH_PM,
        hookData: Bytes::new(),
    }];
    let params0 = ExactInputParams {
        currencyIn: input_cur,
        path,
        minHopPriceX36: vec![],
        amountIn: amount_in,
        amountOutMinimum: min_out,
    }
    .abi_encode();
    let params1 = (input_cur, U256::from(amount_in)).abi_encode_params();
    let params2 = (output_cur, U256::from(min_out)).abi_encode_params();

    let actions = Bytes::from(vec![SWAP_EXACT_IN, SETTLE_ALL, TAKE_ALL]);
    let params: Vec<Bytes> = vec![params0.into(), params1.into(), params2.into()];
    let v4_input = (actions, params).abi_encode_params();

    IUniversalRouter::executeCall {
        commands: Bytes::from(vec![V4_SWAP]),
        inputs: vec![v4_input.into()],
        deadline: U256::from(FAR_DEADLINE),
    }
    .abi_encode()
    .into()
}
