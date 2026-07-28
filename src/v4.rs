// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs
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
pub fn close_liquidity_calldata(token_id: U256, token: Address, owner: Address) -> Bytes {
    // BURN_POSITION(tokenId, amount0Min, amount1Min, hookData),
    // TAKE_PAIR(currency0=ETH, currency1=token, recipient).
    let burn = (token_id, 0u128, 0u128, Bytes::new()).abi_encode_params();
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
