// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs, Inc.
//! Reverse ENS, best-effort, from whichever chain we trade on.
//!
//! ENS lives on Ethereum mainnet, so this asks public mainnet endpoints —
//! the one place this app talks to a chain it does not trade. A name is
//! only cosmetic, so every failure is a silent None; but a reverse record
//! is claimable by anyone, so the name is FORWARD-verified before it is
//! believed — a wallet saying "vitalik.eth" only counts when vitalik.eth
//! says it back.

use alloy::primitives::{keccak256, Address, B256};

const REGISTRY: &str = "0x00000000000C2E074eC69A0dFb2997BA6C7d2e1e";
const ENDPOINTS: [&str; 2] = ["https://ethereum-rpc.publicnode.com", "https://cloudflare-eth.com"];

fn namehash(name: &str) -> B256 {
    let mut node = B256::ZERO;
    if name.is_empty() {
        return node;
    }
    for label in name.rsplit('.') {
        let mut buf = [0u8; 64];
        buf[..32].copy_from_slice(node.as_slice());
        buf[32..].copy_from_slice(keccak256(label.as_bytes()).as_slice());
        node = keccak256(buf);
    }
    node
}

async fn eth_call(rpc: &str, to: &str, data: String) -> Option<String> {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "eth_call",
        "params": [{"to": to, "data": data}, "latest"],
    });
    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(4),
        reqwest::Client::new().post(rpc).json(&body).send(),
    )
    .await
    .ok()?
    .ok()?;
    let v: serde_json::Value = resp.json().await.ok()?;
    v.get("result")?.as_str().map(|s| s.to_string())
}

fn word_addr(hex: &str) -> Option<Address> {
    let h = hex.strip_prefix("0x")?;
    if h.len() < 64 {
        return None;
    }
    h[24..64].parse().ok()
}

fn word_string(hex: &str) -> Option<String> {
    let h = hex.strip_prefix("0x")?;
    let bytes = alloy::primitives::hex::decode(h).ok()?;
    if bytes.len() < 64 {
        return None;
    }
    let len = u64::from_be_bytes(bytes[56..64].try_into().ok()?) as usize;
    let s = bytes.get(64..64 + len)?;
    String::from_utf8(s.to_vec()).ok()
}

async fn resolver_of(rpc: &str, node: B256) -> Option<Address> {
    let data = format!("0x0178b8bf{}", alloy::hex::encode(node));
    word_addr(&eth_call(rpc, REGISTRY, data).await?)
}

/// The verified primary name for `addr`, or None.
pub async fn reverse(addr: Address) -> Option<String> {
    let rev = format!("{:x}.addr.reverse", addr);
    let node = namehash(&rev);
    for rpc in ENDPOINTS {
        let Some(resolver) = resolver_of(rpc, node).await else { continue };
        if resolver.is_zero() {
            return None;
        }
        let data = format!("0x691f3431{}", alloy::hex::encode(node));
        let Some(name) = eth_call(rpc, &format!("{resolver:#x}"), data).await.and_then(|h| word_string(&h)) else {
            continue;
        };
        if name.is_empty() {
            return None;
        }
        let fwd = namehash(&name);
        let Some(fr) = resolver_of(rpc, fwd).await else { continue };
        if fr.is_zero() {
            return None;
        }
        let data = format!("0x3b3b57de{}", alloy::hex::encode(fwd));
        let fwd_addr = eth_call(rpc, &format!("{fr:#x}"), data).await.and_then(|h| word_addr(&h))?;
        if fwd_addr == addr {
            crate::trace(&format!("ens: {addr:#x} is {name}"));
            return Some(name);
        }
        crate::trace(&format!("ens: {addr:#x} claims {name} but the name disagrees — ignored"));
        return None;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference vectors from EIP-137.
    #[test]
    fn namehash_matches_the_eip_vectors() {
        assert_eq!(
            format!("{:#x}", namehash("eth")),
            "0x93cdeb708b7545dc668eb9280176169d1c33cfd8ed6f04690a0bcc88a93fc4ae"
        );
        assert_eq!(
            format!("{:#x}", namehash("foo.eth")),
            "0xde9b09fd7c5f901e23a3f19fecc54828e9c848539801e86591bd9801b019f84f"
        );
    }
}
