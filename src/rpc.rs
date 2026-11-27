//! Thin JSON-RPC layer over alloy: rate limiting, retries, adaptive eth_getLogs
//! and Multicall3 batching. Read-only: there is no signer anywhere in this crate.

use crate::config::RpcCfg;
use crate::events::TOPICS;
use alloy::primitives::{address, Address, Bytes, B256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::{Block, Filter, Log, TransactionReceipt, TransactionRequest};
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{anyhow, Result};
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::{sleep, Instant};
use tracing::{debug, warn};

/// RPC calls that failed (before retry). Shown on the dashboard.
pub static RPC_ERRORS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

const MAX_GAP: Duration = Duration::from_secs(2);

pub const MULTICALL3: Address = address!("cA11bde05977b3631167028862bE2a173976CA11");
const MULTICALL_CHUNK: usize = 150;

sol! {
    struct Call3 { address target; bool allowFailure; bytes callData; }
    struct Result3 { bool success; bytes returnData; }
    function aggregate3(Call3[] calldata calls) external payable returns (Result3[] memory returnData);
}

pub struct Rpc {
    provider: DynProvider,
    min_gap: Duration,
    /// Current spacing between calls: grows on 429, shrinks back on success (like TCP AIMD).
    gap: Mutex<Duration>,
    timeout: Duration,
    next_slot: Mutex<Instant>,
    log_range: Mutex<u64>,
    max_log_range: u64,
}

/// The provider is asking us to slow down (HTTP 429 or a JSON-RPC rate-limit error).
/// Checked before `is_range_error`, because "Too Many Requests" would otherwise
/// look like "too many results" and skip the retry.
fn is_rate_limited(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    ["429", "too many requests", "rate limit", "rate-limit", "ratelimit", "-32005"].iter().any(|k| m.contains(k))
}

/// Errors that mean "ask for fewer blocks", as phrased by common RPC providers.
fn is_range_error(msg: &str) -> bool {
    if is_rate_limited(msg) {
        return false;
    }
    let m = msg.to_ascii_lowercase();
    ["range", "too many", "limit", "exceed", "10000", "response size", "query returned more"]
        .iter()
        .any(|k| m.contains(k))
}

impl Rpc {
    pub fn new(cfg: &RpcCfg) -> Result<Self> {
        let url = cfg.url.parse()?;
        let provider = ProviderBuilder::new().connect_http(url).erased();
        Ok(Self {
            provider,
            timeout: Duration::from_secs(cfg.timeout_sec.max(1)),
            min_gap: Duration::from_secs_f64(1.0 / cfg.max_rps.max(1) as f64),
            gap: Mutex::new(Duration::from_secs_f64(1.0 / cfg.max_rps.max(1) as f64)),
            next_slot: Mutex::new(Instant::now()),
            log_range: Mutex::new(cfg.initial_log_range.max(1)),
            max_log_range: cfg.max_log_range.max(1),
        })
    }

    async fn throttle(&self) {
        let wait_until = {
            let mut slot = self.next_slot.lock().await;
            let now = Instant::now();
            let start = (*slot).max(now);
            *slot = start + *self.gap.lock().await;
            start
        };
        tokio::time::sleep_until(wait_until).await;
    }

    /// Rate limited: double the spacing and pause every caller for `pause`.
    async fn slow_down(&self, pause: Duration) {
        let gap = {
            let mut g = self.gap.lock().await;
            *g = (*g * 2).min(MAX_GAP);
            *g
        };
        let mut slot = self.next_slot.lock().await;
        *slot = (*slot).max(Instant::now() + pause);
        warn!(gap_ms = gap.as_millis() as u64, pause_s = pause.as_secs(), "rpc rate limited, slowing down");
    }

    /// Success: creep back toward the configured rate.
    async fn speed_up(&self) {
        let mut g = self.gap.lock().await;
        if *g > self.min_gap {
            *g = (g.mul_f64(0.97)).max(self.min_gap);
        }
    }

    async fn retry<T, F, Fut>(&self, what: &str, mut f: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let mut delay = Duration::from_millis(500);
        let mut last = None;
        for attempt in 0..10 {
            self.throttle().await;
            let res = match tokio::time::timeout(self.timeout, f()).await {
                Ok(r) => r,
                Err(_) => Err(anyhow!("{what}: timed out")),
            };
            match res {
                Ok(v) => {
                    self.speed_up().await;
                    return Ok(v);
                }
                Err(e) => {
                    let msg = e.to_string();
                    if is_range_error(&msg) {
                        return Err(e);
                    }
                    if is_rate_limited(&msg) {
                        delay = delay.max(Duration::from_secs(2));
                        self.slow_down(delay).await;
                    }
                    RPC_ERRORS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    debug!(what, attempt, error = %msg, "rpc retry");
                    last = Some(e);
                    sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(60));
                }
            }
        }
        Err(last.unwrap_or_else(|| anyhow!("{what}: retries exhausted")))
    }

    pub async fn block_number(&self) -> Result<u64> {
        self.retry("blockNumber", || async { Ok(self.provider.get_block_number().await?) }).await
    }

    pub async fn block(&self, n: u64) -> Result<Block> {
        self.retry("getBlock", || async {
            self.provider
                .get_block_by_number(n.into())
                .await?
                .ok_or_else(|| anyhow!("block {n} not found"))
        })
        .await
    }

    pub async fn block_timestamp(&self, n: u64) -> Result<u64> {
        Ok(self.block(n).await?.header.timestamp)
    }

    /// Find the first block at or after `ts` (binary search on timestamps).
    pub async fn block_at_or_after(&self, ts: u64, latest: u64) -> Result<u64> {
        let (mut lo, mut hi) = (1u64, latest);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.block_timestamp(mid).await? < ts {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok(lo)
    }

    /// All tracked pool logs in [from, to], splitting the range when the provider objects.
    pub async fn pool_logs(&self, from: u64, to: u64) -> Result<Vec<Log>> {
        let mut out = Vec::new();
        let mut start = from;
        while start <= to {
            let range = *self.log_range.lock().await;
            let end = (start + range - 1).min(to);
            let filter = Filter::new().from_block(start).to_block(end).event_signature(TOPICS.to_vec());
            let res = self
                .retry("getLogs", || async { Ok(self.provider.get_logs(&filter).await?) })
                .await;
            match res {
                Ok(logs) => {
                    let n = logs.len();
                    out.extend(logs);
                    start = end + 1;
                    let mut r = self.log_range.lock().await;
                    if n < 2_000 && *r < self.max_log_range {
                        *r = (*r * 2).min(self.max_log_range);
                    }
                }
                Err(e) if is_range_error(&e.to_string()) && range > 1 => {
                    let mut r = self.log_range.lock().await;
                    *r = (range / 2).max(1);
                    warn!(new_range = *r, "getLogs range reduced");
                }
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    pub async fn call(&self, to: Address, data: Bytes) -> Result<Bytes> {
        let tx = TransactionRequest::default().to(to).input(data.into());
        self.retry("eth_call", || {
            let tx = tx.clone();
            async move { Ok(self.provider.call(tx).await?) }
        })
        .await
    }

    /// Batch many view calls through Multicall3. Each result is None if that call reverted.
    pub async fn multicall(&self, calls: Vec<(Address, Bytes)>) -> Result<Vec<Option<Bytes>>> {
        let mut out = Vec::with_capacity(calls.len());
        for chunk in calls.chunks(MULTICALL_CHUNK) {
            let req = aggregate3Call {
                calls: chunk
                    .iter()
                    .map(|(t, d)| Call3 { target: *t, allowFailure: true, callData: d.clone() })
                    .collect(),
            };
            let raw = self.call(MULTICALL3, req.abi_encode().into()).await?;
            let results = aggregate3Call::abi_decode_returns(&raw)?;
            if results.len() != chunk.len() {
                return Err(anyhow!("multicall returned {} results for {} calls", results.len(), chunk.len()));
            }
            out.extend(results.into_iter().map(|r| r.success.then_some(r.returnData)));
        }
        Ok(out)
    }

    pub async fn tx_and_receipt(&self, hash: B256) -> Result<Option<(Address, TransactionReceipt)>> {
        let receipt = self
            .retry("getReceipt", || async { Ok(self.provider.get_transaction_receipt(hash).await?) })
            .await?;
        Ok(receipt.map(|r| (r.from, r)))
    }

    /// Every receipt in one block (one call). Used to count failed attempts.
    pub async fn block_receipts(&self, n: u64) -> Result<Option<Vec<TransactionReceipt>>> {
        self.retry("getBlockReceipts", || async { Ok(self.provider.get_block_receipts(n.into()).await?) })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_errors_are_recognised() {
        assert!(is_range_error("query returned more than 10000 results"));
        assert!(is_range_error("block range too large"));
        assert!(is_range_error("Log response size exceeded"));
        assert!(!is_range_error("connection reset by peer"));
    }

    #[test]
    fn rate_limits_are_not_range_errors() {
        let m = r#"HTTP error 429 with body: {"jsonrpc":"2.0","error":{"code":429,"message":"Too Many Requests"}}"#;
        assert!(is_rate_limited(m));
        assert!(!is_range_error(m));
        assert!(is_rate_limited("daily request rate limit exceeded"));
        assert!(!is_rate_limited("block range too large"));
    }
}
