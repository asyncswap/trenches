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

/// Build UniversalRouter.execute() calldata for a Flaunch coin: a two-hop v4
/// exact-in through flETH, since Flaunch pools pair against flETH rather than
/// native ETH. buy = ETH -> flETH (flETH hook pool) -> coin (Flaunch hook
/// pool), with `amount_in` sent as value; sell runs the same path backwards
/// and settles the coin through Permit2.
pub fn flaunch_swap_calldata(token: Address, buy: bool, amount_in: u128, min_out: u128) -> Bytes {
    let hop = |currency: Address, hooks: Address| PathKey {
        intermediateCurrency: currency,
        // The real pool fee is 0 — Flaunch charges its cut in the hook.
        fee: alloy::primitives::aliases::U24::ZERO,
        tickSpacing: FLAUNCH_TICK_SPACING.try_into().unwrap(),
        hooks,
        hookData: Bytes::new(),
    };
    let (input_cur, output_cur, path) = if buy {
        (Address::ZERO, token, vec![hop(FLETH, FLETH_HOOKS), hop(token, FLAUNCH_PM)])
    } else {
        (token, Address::ZERO, vec![hop(FLETH, FLAUNCH_PM), hop(Address::ZERO, FLETH_HOOKS)])
    };

    let params0 = ExactInputParams {
        currencyIn: input_cur,
        path,
        minHopPriceX36: vec![], // no per-hop limits; amountOutMinimum guards the trade
        amountIn: amount_in,
        amountOutMinimum: min_out,
    }
    .abi_encode();

    // SETTLE_ALL(inputCurrency, amountIn), TAKE_ALL(outputCurrency, minOut).
    let params1 = (input_cur, U256::from(amount_in)).abi_encode_params();
    let params2 = (output_cur, U256::from(min_out)).abi_encode_params();

    let actions = Bytes::from(vec![SWAP_EXACT_IN, SETTLE_ALL, TAKE_ALL]);
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

/// One v4 hop between a coin and its quote, through a named hook.
///
/// For a pons v2 pool: the hook is part of the pool key, so it cannot be
/// defaulted — a swap built with hooks = 0 addresses a pool that does not
/// exist. The pool's own fee is ZERO because the hook charges instead, which
/// is why fee is not a parameter and why reading the pool's fee tells you
/// nothing about what a trade costs.
pub fn hop_calldata(
    token: Address,
    quote: Address,
    hook: Address,
    tick_spacing: i32,
    buy: bool,
    amount_in: u128,
    min_out: u128,
) -> Bytes {
    let (input_cur, output_cur, hop_cur) =
        if buy { (quote, token, token) } else { (token, quote, quote) };
    let path = vec![PathKey {
        intermediateCurrency: hop_cur,
        fee: alloy::primitives::aliases::U24::ZERO,
        tickSpacing: tick_spacing.try_into().unwrap_or_default(),
        hooks: hook,
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

    // Golden fixtures generated with viem's encodeAbiParameters using the
    // Flaunch SDK's exact ABI shapes (universalRouter.ts, the robinhood
    // hop-price-limit variant) for the same token/amounts. Byte equality here
    // means the router parses our calldata exactly as it parses the SDK's.
    const SDK_BUY: &str = "0x3593564c000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000f4865700000000000000000000000000000000000000000000000000000000000000000110000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000004a0000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000003070c0f000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000030000000000000000000000000000000000000000000000000000000000000060000000000000000000000000000000000000000000000000000000000000034000000000000000000000000000000000000000000000000000000000000003a000000000000000000000000000000000000000000000000000000000000002c00000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a0000000000000000000000000000000000000000000000000000000000000028000000000000000000000000000000000000000000000000000038d7ea4c68000000000000000000000000000000000000000000000000000ab54a98ceb1f0ad200000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000043c1117dafa3a3d0c7148eb48b301300000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003c000000000000000000000000ea22ae03085caf74ac3393f9902539fbe978688800000000000000000000000000000000000000000000000000000000000000a0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000011111111111111111111111111111111111111110000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003c0000000000000000000000005cf8e499c7c466c7e2cf127bdf129f57151e65dc00000000000000000000000000000000000000000000000000000000000000a0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000038d7ea4c6800000000000000000000000000000000000000000000000000000000000000000400000000000000000000000001111111111111111111111111111111111111111000000000000000000000000000000000000000000000000ab54a98ceb1f0ad2";
    const SDK_SELL: &str = "0x3593564c000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000f4865700000000000000000000000000000000000000000000000000000000000000000110000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000004a0000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000003070c0f000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000030000000000000000000000000000000000000000000000000000000000000060000000000000000000000000000000000000000000000000000000000000034000000000000000000000000000000000000000000000000000000000000003a000000000000000000000000000000000000000000000000000000000000002c00000000000000000000000000000000000000000000000000000000000000020000000000000000000000000111111111111111111111111111111111111111100000000000000000000000000000000000000000000000000000000000000a0000000000000000000000000000000000000000000000000000000000000028000000000000000000000000000000000000000000000000000038d7ea4c68000000000000000000000000000000000000000000000000000ab54a98ceb1f0ad200000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000043c1117dafa3a3d0c7148eb48b301300000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003c0000000000000000000000005cf8e499c7c466c7e2cf127bdf129f57151e65dc00000000000000000000000000000000000000000000000000000000000000a0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003c000000000000000000000000ea22ae03085caf74ac3393f9902539fbe978688800000000000000000000000000000000000000000000000000000000000000a0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000040000000000000000000000000111111111111111111111111111111111111111100000000000000000000000000000000000000000000000000038d7ea4c6800000000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000ab54a98ceb1f0ad2";

    #[test]
    fn flaunch_calldata_matches_sdk_fixture() {
        let buy = flaunch_swap_calldata(TOKEN, true, AMOUNT_IN, MIN_OUT);
        assert_eq!(format!("0x{}", alloy::hex::encode(&buy)), SDK_BUY);
        let sell = flaunch_swap_calldata(TOKEN, false, AMOUNT_IN, MIN_OUT);
        assert_eq!(format!("0x{}", alloy::hex::encode(&sell)), SDK_SELL);
    }

    #[test]
    fn flaunch_calldata_decodes_back() {
        for buy in [true, false] {
            let data = flaunch_swap_calldata(TOKEN, buy, AMOUNT_IN, MIN_OUT);
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
            assert_eq!(p.path.len(), 2);
            let spacing: alloy::primitives::aliases::I24 =
                FLAUNCH_TICK_SPACING.try_into().unwrap();
            for hop in &p.path {
                assert_eq!(hop.fee, alloy::primitives::aliases::U24::ZERO);
                assert_eq!(hop.tickSpacing, spacing);
            }
            if buy {
                assert_eq!(p.currencyIn, Address::ZERO);
                assert_eq!((p.path[0].intermediateCurrency, p.path[0].hooks), (FLETH, FLETH_HOOKS));
                assert_eq!((p.path[1].intermediateCurrency, p.path[1].hooks), (TOKEN, FLAUNCH_PM));
            } else {
                assert_eq!(p.currencyIn, TOKEN);
                assert_eq!((p.path[0].intermediateCurrency, p.path[0].hooks), (FLETH, FLAUNCH_PM));
                assert_eq!((p.path[1].intermediateCurrency, p.path[1].hooks), (Address::ZERO, FLETH_HOOKS));
            }
        }
    }
}

#[cfg(test)]
mod _probe2 {
    use super::*;
    #[test]
    fn dump_pair() {
        let t: Address = "0x8a5B9feb491e6C344Dd2702DcC056f60C354230B".parse().unwrap();
        let a = 854_537_310_374_830u128;
        for (tag, m) in [("OURS", 10_410_711_766_844_823_172_022_272u128),
                         ("THEIRS", 10_197_463_432_112_615_282_331_528u128)] {
            println!("{tag}=0x{}", alloy::hex::encode(flaunch_swap_calldata(t, true, a, m)));
        }
    }
}
