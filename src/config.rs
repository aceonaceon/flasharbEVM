use alloy::primitives::Address;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub rpc: RpcCfg,
    pub run: RunCfg,
    pub detect: DetectCfg,
    pub gate: GateCfg,
    pub tokens: TokensCfg,
    pub factories: Vec<FactoryCfg>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RpcCfg {
    pub url: String,
    pub max_rps: u32,
    pub initial_log_range: u64,
    pub max_log_range: u64,
    pub timeout_sec: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RunCfg {
    pub db_path: String,
    pub poll_ms: u64,
    pub backfill_hours: u64,
    pub warmup_minutes: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DetectCfg {
    pub min_gross_usd: f64,
    pub gas_cost_usd: f64,
    pub suspect_gap_pct: f64,
    pub min_pool_liquidity_usd: f64,
    pub max_closer_tx_lookups: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GateCfg {
    pub min_median_gross_usd: f64,
    pub min_addressable_usd_per_day: f64,
    pub max_same_block_share: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokensCfg {
    pub weth: Address,
    pub stables: Vec<Address>,
    pub majors: Vec<Address>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactoryKind {
    Univ3,
    Algebra,
    AeroCl,
    Univ2,
    AeroV2,
}

impl FactoryKind {
    pub fn is_cl(self) -> bool {
        matches!(self, Self::Univ3 | Self::Algebra | Self::AeroCl)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct FactoryCfg {
    pub name: String,
    pub kind: FactoryKind,
    pub address: Address,
    #[serde(default)]
    pub fee_bps: Option<u32>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let mut cfg: Config = toml::from_str(&raw).context("parsing config")?;
        if let Ok(url) = std::env::var("FLASHARB_RPC_URL") {
            if !url.is_empty() {
                cfg.rpc.url = url;
            }
        }
        Ok(cfg)
    }

    pub fn factory(&self, addr: Address) -> Option<&FactoryCfg> {
        self.factories.iter().find(|f| f.address == addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_config_parses() {
        let cfg = Config::load(Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/config.toml")))
            .expect("config.toml must parse");
        assert!(cfg.factories.len() >= 4);
        assert!(cfg.factories.iter().any(|f| f.kind == FactoryKind::AeroCl));
        assert_eq!(cfg.tokens.stables.len(), 3);
    }
}
