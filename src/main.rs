//! flasharb-shadow: records Arbitrum DEX arbitrage opportunities and who took
//! them. It never signs or sends a transaction and holds no keys.

mod amm;
mod config;
mod dashboard;
mod detector;
mod events;
mod ledger;
mod pools;
mod price;
mod report;
mod rpc;
mod web;

use alloy::primitives::{Address, B256};
use anyhow::Result;
use clap::{Parser, Subcommand};
use config::Config;
use detector::{CloseKind, Detector};
use ledger::{CloserTx, Ledger};
use price::Oracle;
use rpc::Rpc;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

#[derive(Parser)]
#[command(version, about = "FlashArb shadow recorder (records only, never sends)")]
struct Cli {
    #[arg(long, default_value = "config.toml")]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Follow the chain and record opportunities (default).
    Run,
    /// Serve only the dashboard over an existing ledger (no chain access).
    Serve,
    /// Print the markdown report from the ledger.
    Report {
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

/// A block range with the timestamps of its ends, used to date opportunities.
#[derive(Debug, Clone, Copy)]
struct BlockSpan {
    from: u64,
    to: u64,
    from_ts: u64,
    to_ts: u64,
}

impl BlockSpan {
    /// Linear interpolation between the two end timestamps (Arbitrum block times are near-constant).
    fn ts_of(&self, block: u64) -> u64 {
        if self.to <= self.from || self.to_ts <= self.from_ts {
            return self.to_ts;
        }
        let b = block.clamp(self.from, self.to) - self.from;
        self.from_ts + (self.to_ts - self.from_ts) * b / (self.to - self.from)
    }
}

/// Classify pools in batches of this size.
const RESOLVE_BATCH: usize = 100;
/// Competitor tx lookups per loop iteration.
const ENRICH_PER_LOOP: usize = 3;

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::BlockSpan;

    #[test]
    fn block_span_interpolates() {
        let s = BlockSpan { from: 100, to: 200, from_ts: 1_000, to_ts: 1_025 };
        assert_eq!(s.ts_of(100), 1_000);
        assert_eq!(s.ts_of(200), 1_025);
        assert_eq!(s.ts_of(140), 1_010);
        assert_eq!(s.ts_of(50), 1_000);
        let one = BlockSpan { from: 5, to: 5, from_ts: 9, to_ts: 10 };
        assert_eq!(one.ts_of(5), 10);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let cli = Cli::parse();
    let cfg = Config::load(&cli.config)?;
    match cli.cmd.unwrap_or(Cmd::Run) {
        Cmd::Report { out } => {
            let ledger = Ledger::open(cfg.run.db_path.as_ref())?;
            let md = report::render(&ledger.conn, &cfg)?;
            match out {
                Some(p) => std::fs::write(p, md)?,
                None => println!("{md}"),
            }
            Ok(())
        }
        Cmd::Serve => {
            Ledger::open(cfg.run.db_path.as_ref())?;
            let (bind, db) = (cfg.web.bind.clone(), PathBuf::from(&cfg.run.db_path));
            web::serve(&bind, db, cfg).await
        }
        Cmd::Run => run(cfg).await,
    }
}

struct Runner {
    cfg: Config,
    rpc: Rpc,
    ledger: Ledger,
    det: Detector,
    /// Every pool we've classified (verified or not), so we never ask twice.
    classified: HashSet<Address>,
    /// Factory of each rejected pool, for the "unknown factories" table.
    rejected_factory: HashMap<Address, Address>,
    enrich_queue: VecDeque<(i64, B256)>,
    enrich_done: u64,
    base_fee_cache: HashMap<u64, u128>,
}

async fn run(cfg: Config) -> Result<()> {
    let rpc = Rpc::new(&cfg.rpc)?;
    let ledger = Ledger::open(cfg.run.db_path.as_ref())?;
    let oracle = Oracle::new(cfg.tokens.weth, cfg.tokens.stables.clone());
    let mut det = Detector::new(cfg.detect.clone(), oracle, cfg.tokens.majors.clone());
    let want = pools::CLASSIFIER_VERSION.to_string();
    if ledger.meta("classifier_version")?.as_deref() != Some(want.as_str()) {
        if ledger.last_block()?.is_some() {
            warn!(version = %want, "pool classification rules changed: rebuilding the ledger from scratch");
        }
        ledger.reset()?;
        ledger.set_meta("classifier_version", &want)?;
    }
    let mut classified = HashSet::new();
    let mut rejected_factory = HashMap::new();
    for row in ledger.load_pools()? {
        classified.insert(row.info.address);
        if row.verified {
            det.add_pool(row.info);
        } else {
            rejected_factory.insert(row.info.address, row.factory);
        }
    }
    let enrich_done = ledger.closer_tx_count()?;
    if cfg.web.enabled {
        let (bind, db, c) = (cfg.web.bind.clone(), PathBuf::from(&cfg.run.db_path), cfg.clone());
        tokio::spawn(async move {
            if let Err(e) = web::serve(&bind, db, c).await {
                warn!(error = %e, "dashboard stopped");
            }
        });
    }
    info!(pools = classified.len(), "loaded pool registry");
    let mut r = Runner {
        cfg,
        rpc,
        ledger,
        det,
        classified,
        rejected_factory,
        enrich_queue: VecDeque::new(),
        enrich_done,
        base_fee_cache: HashMap::new(),
    };
    r.main_loop().await
}

impl Runner {
    async fn start_block(&self, latest: u64) -> Result<u64> {
        if let Some(b) = self.ledger.last_block()? {
            return Ok(b + 1);
        }
        if self.cfg.run.backfill_hours == 0 {
            return Ok(latest);
        }
        let now = self.rpc.block_timestamp(latest).await?;
        let ts = now.saturating_sub(self.cfg.run.backfill_hours * 3600);
        let b = self.rpc.block_at_or_after(ts, latest).await?;
        info!(from = b, hours = self.cfg.run.backfill_hours, "backfilling");
        Ok(b)
    }

    async fn main_loop(&mut self) -> Result<()> {
        let latest = self.rpc.block_number().await?;
        let mut next = self.start_block(latest).await?;
        // In-memory state is empty after every start, so warm up before recording.
        let start_ts = self.rpc.block_timestamp(next).await?;
        let warm_until = start_ts + self.cfg.run.warmup_minutes * 60;
        self.det.recording = false;
        let mut prev_ts = start_ts;
        let mut last_log = std::time::Instant::now();
        let mut last_report = std::time::Instant::now();
        let poll = Duration::from_millis(self.cfg.run.poll_ms);
        let stop = tokio::signal::ctrl_c();
        tokio::pin!(stop);

        loop {
            let latest = tokio::select! {
                _ = &mut stop => { info!("shutting down"); return Ok(()); }
                b = self.rpc.block_number() => match b {
                    Ok(b) => b,
                    Err(e) => {
                        // Never let a transient RPC failure end the process: a restart
                        // forgets every open opportunity and re-warms for 10 minutes.
                        warn!(error = %e, "blockNumber failed, retrying");
                        tokio::time::sleep(Duration::from_secs(10)).await;
                        continue;
                    }
                },
            };
            if next > latest {
                self.enrich().await;
                tokio::time::sleep(poll).await;
                continue;
            }
            web::CHAIN_HEAD.store(latest, std::sync::atomic::Ordering::Relaxed);
            let to = latest.min(next + self.cfg.rpc.max_log_range - 1);
            let to_ts = match self.rpc.block_timestamp(to).await {
                Ok(t) => t,
                Err(e) => {
                    warn!(error = %e, "block timestamp failed, retrying");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };
            let span = BlockSpan { from: next, to, from_ts: prev_ts, to_ts };
            let n_events = match self.process(span).await {
                Ok(n) => n,
                Err(e) => {
                    warn!(error = %e, from = next, to, "batch failed, retrying");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };
            self.ledger.add_coverage(next, to, prev_ts, to_ts, self.det.recording, n_events)?;
            self.ledger.set_last_block(to)?;
            prev_ts = to_ts;
            web::LAST_TS.store(to_ts, std::sync::atomic::Ordering::Relaxed);
            web::RECORDING.store(self.det.recording, std::sync::atomic::Ordering::Relaxed);
            if !self.det.recording && to_ts >= warm_until {
                self.det.recording = true;
                info!(block = to, "warm-up done, recording");
            }
            next = to + 1;
            self.enrich().await;

            if last_log.elapsed() > Duration::from_secs(60) {
                info!(
                    block = to,
                    behind = latest.saturating_sub(to),
                    pools = self.classified.len(),
                    open = self.det.open_count(),
                    "progress"
                );
                last_log = std::time::Instant::now();
            }
            if last_report.elapsed() > Duration::from_secs(3600) {
                self.write_report();
                last_report = std::time::Instant::now();
            }
        }
    }

    fn write_report(&self) {
        let path = PathBuf::from(&self.cfg.run.db_path).with_file_name("report.md");
        match report::render(&self.ledger.conn, &self.cfg).map(|md| std::fs::write(&path, md)) {
            Ok(Ok(())) => info!(path = %path.display(), "report written"),
            Ok(Err(e)) => warn!(error = %e, "report write failed"),
            Err(e) => warn!(error = %e, "report render failed"),
        }
    }

    /// Fetch, classify, detect and record one block range. Returns the number of events.
    async fn process(&mut self, span: BlockSpan) -> Result<u64> {
        let logs = self.rpc.pool_logs(span.from, span.to).await?;
        let mut evs: Vec<_> = logs.iter().filter_map(events::decode).collect();
        evs.sort_by_key(|e| (e.block, e.tx_index, e.log_index));

        let unknown: Vec<Address> = {
            let mut seen = HashSet::new();
            evs.iter()
                .map(|e| e.pool)
                .filter(|p| !self.classified.contains(p) && seen.insert(*p))
                .collect()
        };
        for chunk in unknown.chunks(RESOLVE_BATCH) {
            for row in pools::resolve(&self.rpc, &self.cfg, chunk).await? {
                self.ledger.upsert_pool(&row)?;
                self.classified.insert(row.info.address);
                if row.verified {
                    self.det.add_pool(row.info);
                } else {
                    self.rejected_factory.insert(row.info.address, row.factory);
                }
            }
        }

        let mut unknown_counts: HashMap<Address, u64> = HashMap::new();
        let mut finished = Vec::new();
        for ev in &evs {
            if let Some(f) = self.rejected_factory.get(&ev.pool) {
                *unknown_counts.entry(*f).or_default() += 1;
                continue;
            }
            finished.extend(self.det.on_event(ev));
        }
        // `to` is a whole block, so the last tx is complete.
        finished.extend(self.det.finish_tx());

        let now = now_secs();
        let tx = self.ledger.conn.unchecked_transaction()?;
        for o in &finished {
            let id = self.ledger.insert_opportunity(o, now, span.ts_of(o.open_block) as i64)?;
            let wanted = self.enrich_done + self.enrich_queue.len() as u64;
            if o.close_kind == CloseKind::Arb && !o.suspect && wanted < self.cfg.detect.max_closer_tx_lookups {
                if let Some(h) = o.close_tx {
                    self.enrich_queue.push_back((id, h));
                }
            }
        }
        for (f, n) in unknown_counts {
            if f != Address::ZERO {
                self.ledger.bump_unknown_factory(f, n)?;
            }
        }
        tx.commit()?;
        Ok(evs.len() as u64)
    }

    /// Look up a few competitor arbitrage txs: who sent them and what they paid.
    async fn enrich(&mut self) {
        for _ in 0..ENRICH_PER_LOOP {
            let Some((opp_id, hash)) = self.enrich_queue.pop_front() else { return };
            match self.enrich_one(opp_id, hash).await {
                Ok(()) => self.enrich_done += 1,
                Err(e) => warn!(error = %e, tx = %hash, "closer lookup failed"),
            }
        }
    }

    async fn enrich_one(&mut self, opp_id: i64, hash: B256) -> Result<()> {
        let Some((from, rc)) = self.rpc.tx_and_receipt(hash).await? else { return Ok(()) };
        let block = rc.block_number.unwrap_or_default();
        let base_fee = match self.base_fee_cache.get(&block) {
            Some(b) => *b,
            None => {
                let b = self.rpc.block(block).await?.header.base_fee_per_gas.unwrap_or(0) as u128;
                if self.base_fee_cache.len() > 10_000 {
                    self.base_fee_cache.clear();
                }
                self.base_fee_cache.insert(block, b);
                b
            }
        };
        let egp = rc.effective_gas_price;
        let gas = rc.gas_used;
        let priority_wei = egp.saturating_sub(base_fee) as f64 * gas as f64;
        let total_wei = egp as f64 * gas as f64;
        let weth = self.cfg.tokens.weth;
        let usd = |wei: f64| self.det.oracle.usd(weth, wei).unwrap_or(0.0);
        self.ledger.insert_closer_tx(&CloserTx {
            tx: hash,
            opp_id,
            from,
            gas_used: gas,
            effective_gas_price: egp,
            base_fee,
            priority_usd: usd(priority_wei),
            total_fee_usd: usd(total_wei),
        })?;
        Ok(())
    }
}
