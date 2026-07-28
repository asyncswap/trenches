// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 AsyncSwap Labs
//! Live USD price feed (CoinGecko). Used to value quote currencies — ETH for
//! native pools, and stablecoins for stable-quoted pools. Stables are NOT
//! assumed to be $1: USDG, for instance, drifts, so we fetch it like any other
//! asset. Best-effort: a failed fetch leaves the previous estimate in place.

use std::collections::HashMap;

/// CoinGecko `simple/price` for a set of coin ids -> {id: usd}. Returns an empty
/// map on any network/parse error (caller keeps its previous values).
pub async fn fetch_usd(ids: &[&str]) -> HashMap<String, f64> {
    let mut out = HashMap::new();
    if ids.is_empty() {
        return out;
    }
    let url = format!(
        "https://api.coingecko.com/api/v3/simple/price?ids={}&vs_currencies=usd",
        ids.join(",")
    );
    let resp = match reqwest::Client::new()
        .get(&url)
        .header("accept", "application/json")
        .timeout(std::time::Duration::from_secs(6))
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return out,
    };
    let json: serde_json::Value = match resp.json().await {
        Ok(j) => j,
        Err(_) => return out,
    };
    if let Some(obj) = json.as_object() {
        for (id, v) in obj {
            if let Some(usd) = v.get("usd").and_then(|x| x.as_f64()) {
                out.insert(id.clone(), usd);
            }
        }
    }
    out
}
