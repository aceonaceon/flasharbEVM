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
    #[serde(default)]
    pub web: WebCfg,
    #[serde(default)]
    pub discovery: DiscoveryCfg,
    #[serde(default)]
    pub chain: ChainCfg,
}

/// Which chain this instance watches; only used for labels and explorer links.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct ChainCfg {
    pub name: String,
    pub explorer: String,
}

impl Default for ChainCfg {
    fn default() -> Self {
        Self { name: "Arbitrum".into(), explorer: "https://arbiscan.io".into() }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DiscoveryCfg {
    /// Accept pools from factories not listed in config when their type is unambiguous.
    #[serde(default = "default_true")]
    pub auto_accept_unknown_factories: bool,
}

impl Default for DiscoveryCfg {
    fn default() -> Self {
        Self { auto_accept_unknown_factories: true }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct WebCfg {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_bind")]
    pub bind: String,
    /// Other chains' recorders whose summaries this dashboard can show (proxied, read-only).
    #[serde(default)]
    pub peers: Vec<PeerCfg>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PeerCfg {
    pub name: String,
    /// Base URL of the peer's dashboard, e.g. http://shadow-bsc:8088
    pub url: String,
}

impl Default for WebCfg {
    fn default() -> Self {
        Self { enabled: true, bind: default_bind(), peers: Vec::new() }
    }
}

fn default_true() -> bool {
    true
}

fn default_bind() -> String {
    "0.0.0.0:8088".into()
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
    /// An opportunity still open after this many blocks is closed as stale (~10 minutes).
    #[serde(default = "default_stale_blocks")]
    pub stale_blocks: u64,
    /// What one failed attempt would cost us (USD). 0 where failures are free
    /// (Arbitrum conditional txs, private bundles); a little gas on Base / Optimism / Polygon.
    #[serde(default)]
    pub fail_cost_usd: f64,
}

fn default_stale_blocks() -> u64 {
    crate::detector::STALE_BLOCKS
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
    /// Stablecoins with 6 decimals.
    pub stables: Vec<Address>,
    /// Stablecoins with 18 decimals (BSC's USDT/USDC, DAI).
    #[serde(default)]
    pub stables18: Vec<Address>,
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
        assert_eq!(cfg.chain.name, "Arbitrum");
        assert_eq!(cfg.web.peers.len(), 5);
        assert_eq!(cfg.detect.fail_cost_usd, 0.0);
    }

    #[test]
    fn chain_configs_parse() {
        for (file, name, stale) in [
            ("bsc.toml", "BSC", 800),
            ("ethereum.toml", "Ethereum", 50),
            ("base.toml", "Base", 300),
            ("optimism.toml", "Optimism", 300),
            ("polygon.toml", "Polygon", 300),
        ] {
            let p = format!("{}/configs/{file}", env!("CARGO_MANIFEST_DIR"));
            let cfg = Config::load(Path::new(&p)).unwrap_or_else(|e| panic!("{file}: {e:#}"));
            assert_eq!(cfg.chain.name, name);
            assert_eq!(cfg.detect.stale_blocks, stale);
            assert!(!cfg.factories.is_empty());
            assert!(cfg.web.peers.is_empty());
        }
        let bsc = Config::load(Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/configs/bsc.toml"))).unwrap();
        assert_eq!(bsc.tokens.stables18.len(), 2);
        let base = Config::load(Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/configs/base.toml"))).unwrap();
        assert!(base.detect.fail_cost_usd > 0.0);
    }
}
