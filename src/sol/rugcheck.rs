//! RugCheck token risk scoring.
//!
//! Answers the question the trenches can't: *is this coin's creator a serial
//! rugger, is LP locked, can the mint still be inflated*. The most valuable
//! field is creator rug history — the deployer-reputation signal that would
//! otherwise need building from scratch.
//!
//! Risk is advisory, never a gate: it's third-party opinion, it can be stale,
//! and a clean score is not a safety guarantee. It's surfaced next to the trade,
//! not used to block one.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Deserialize;

use crate::view::Tone;

/// One flagged risk from a report.
#[derive(Debug, Clone, Deserialize)]
pub struct Risk {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub score: i64,
    /// `danger` | `warn` | `info`.
    #[serde(default)]
    pub level: String,
}

/// A token's risk summary.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Report {
    #[serde(default)]
    pub risks: Vec<Risk>,
    /// Normalised 0-100. RugCheck scores RISK, so higher is worse.
    #[serde(default, rename = "score_normalised")]
    pub score: u32,
    #[serde(default, rename = "lpLockedPct")]
    pub lp_locked_pct: f64,
}

impl Report {
    /// True if anything is flagged at `danger`.
    pub fn has_danger(&self) -> bool {
        self.risks.iter().any(|r| r.level.eq_ignore_ascii_case("danger"))
    }

    /// The single most severe risk, for a one-line summary.
    pub fn worst(&self) -> Option<&Risk> {
        self.risks.iter().max_by_key(|r| r.score)
    }

    /// Compact badge for tables: score plus a marker when something is flagged.
    pub fn badge(&self) -> String {
        if self.risks.is_empty() {
            format!("{}", self.score)
        } else if self.has_danger() {
            format!("{} ⚠", self.score)
        } else {
            format!("{} !", self.score)
        }
    }

    /// Colour for the badge. `warn_score` comes from config.
    pub fn tone(&self, warn_score: u32) -> Tone {
        if self.has_danger() {
            Tone::Bad
        } else if self.score >= warn_score || !self.risks.is_empty() {
            Tone::Warn
        } else {
            Tone::Good
        }
    }

    /// One-line human summary for the market panel.
    pub fn summary(&self) -> String {
        match self.worst() {
            Some(r) => format!("{} — {}", self.score, r.name),
            None if self.score == 0 => "no data".into(),
            None => format!("{} — no flags", self.score),
        }
    }
}

/// Cached RugCheck client.
///
/// Reports change slowly (creator history, LP locks) but the trenches re-render
/// several times a second, so results are cached per mint. Without that the free
/// tier would be exhausted in seconds.
#[derive(Clone)]
pub struct RugCheck {
    base_url: String,
    key: Option<String>,
    enabled: bool,
    http: reqwest::Client,
    cache: Arc<Mutex<HashMap<String, Report>>>,
}

impl RugCheck {
    pub fn new(cfg: &crate::config::RugCheck) -> RugCheck {
        RugCheck {
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            // Prefer the full key; the shielded one is rate-limited per IP.
            key: cfg.api_key.clone().or_else(|| cfg.shield_key.clone()),
            enabled: cfg.enabled,
            http: reqwest::Client::new(),
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// A cached report, or `None` if unknown/disabled. Never fetches.
    pub fn cached(&self, mint: &str) -> Option<Report> {
        self.cache.lock().ok()?.get(mint).cloned()
    }

    /// Fetch (or return cached) a report for `mint`.
    ///
    /// Returns `None` on any failure — a scoring outage must never block trading
    /// or surface as a scary-looking zero score.
    pub async fn report(&self, mint: &str) -> Option<Report> {
        if !self.enabled {
            return None;
        }
        if let Some(hit) = self.cached(mint) {
            return Some(hit);
        }
        let mut url = format!("{}/v1/tokens/{mint}/report/summary", self.base_url);
        if let Some(k) = &self.key {
            url.push_str(&format!("?key={k}"));
        }
        let resp = tokio::time::timeout(std::time::Duration::from_secs(6), self.http.get(&url).send())
            .await
            .ok()?
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let report: Report = resp.json().await.ok()?;
        if let Ok(mut c) = self.cache.lock() {
            // Bound the cache — the trenches stream new mints indefinitely.
            if c.len() > 500 {
                c.clear();
            }
            c.insert(mint.to_string(), report.clone());
        }
        Some(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(score: u32, levels: &[&str]) -> Report {
        Report {
            score,
            lp_locked_pct: 100.0,
            risks: levels
                .iter()
                .enumerate()
                .map(|(i, l)| Risk {
                    name: format!("risk {i}"),
                    description: String::new(),
                    score: (i as i64 + 1) * 100,
                    level: l.to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn danger_dominates_the_tone() {
        assert_eq!(report(10, &["danger"]).tone(40), Tone::Bad);
        // A low score with a danger flag is still bad — the flag wins.
        assert!(report(5, &["danger"]).has_danger());
        assert_eq!(report(90, &["warn"]).tone(40), Tone::Warn);
        assert_eq!(report(5, &[]).tone(40), Tone::Good);
        // Above threshold with no flags is still a warning.
        assert_eq!(report(50, &[]).tone(40), Tone::Warn);
    }

    #[test]
    fn worst_risk_is_the_highest_scoring() {
        let r = report(50, &["info", "danger", "warn"]);
        assert_eq!(r.worst().map(|x| x.score), Some(300));
    }

    #[test]
    fn badges_signal_flags() {
        assert_eq!(report(3, &[]).badge(), "3");
        assert!(report(50, &["danger"]).badge().contains('⚠'));
        assert!(report(20, &["warn"]).badge().contains('!'));
    }

    #[test]
    fn summary_handles_the_empty_case() {
        assert_eq!(Report::default().summary(), "no data");
        assert!(report(12, &[]).summary().contains("no flags"));
        assert!(report(99, &["danger"]).summary().contains("risk 0"));
    }

    /// Live check against the real API.
    ///   cargo test --features solana live_rugcheck -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn live_rugcheck_scores_a_real_coin() {
        let cfg = crate::config::RugCheck::default();
        let rc = RugCheck::new(&cfg);
        // USDC: should score clean.
        let r = rc.report("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v").await;
        println!("USDC -> {r:?}");
        let r = r.expect("USDC should return a report");
        assert!(!r.has_danger(), "USDC must not be flagged as danger");
        // Second call must hit the cache.
        assert!(rc.cached("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v").is_some());
    }
}
