//! Opportunity detector. Pure logic, no I/O, so it is fully unit-testable.
//!
//! Events are applied in chain order. Pairs are only evaluated at transaction
//! boundaries: a multi-hop router trade moves several pools inside one tx, and
//! the intermediate states would otherwise look like fake price gaps.
//!
//! An opportunity opens when two verified pools of the same pair show a gross
//! arbitrage profit (after both swap fees) above `min_gross_usd`. It closes at
//! the first transaction after which the profit falls below half that level.
//! The closing transaction is classified as:
//! - `Arb`: it touched both pools, i.e. someone else took the arbitrage;
//! - `Organic`: ordinary flow moved one pool back;
//! - `Stale`: still open after `STALE_BLOCKS` (tick-range limits, a bad token, etc.).

use crate::amm::{best_arb, gap_pct, Reserves};
use crate::config::DetectCfg;
use crate::events::{PoolEvent, Update};
use crate::price::Oracle;
use alloy::primitives::{Address, B256};
use std::collections::{HashMap, HashSet};

/// About 10 minutes of Arbitrum blocks.
pub const STALE_BLOCKS: u64 = 2_400;

#[derive(Debug, Clone)]
pub struct PoolInfo {
    pub address: Address,
    pub venue: String,
    pub token0: Address,
    pub token1: Address,
    pub fee_ppm: u32,
    pub is_cl: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseKind {
    Arb,
    Organic,
    Stale,
}

impl CloseKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Arb => "arb",
            Self::Organic => "organic",
            Self::Stale => "stale",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Opportunity {
    pub pool_a: Address,
    pub pool_b: Address,
    pub venue_a: String,
    pub venue_b: String,
    pub token0: Address,
    pub token1: Address,
    pub major: bool,
    pub open_block: u64,
    pub open_tx: B256,
    pub gross_usd: f64,
    pub peak_usd: f64,
    pub gap_pct: f64,
    pub suspect: bool,
    pub min_depth_usd: f64,
    pub close_block: u64,
    pub close_tx: Option<B256>,
    pub close_kind: CloseKind,
    pub closer_sender: Option<Address>,
}

impl Opportunity {
    pub fn delay_blocks(&self) -> u64 {
        self.close_block.saturating_sub(self.open_block)
    }
    pub fn same_block(&self) -> bool {
        self.close_kind != CloseKind::Stale && self.close_block == self.open_block
    }
}

struct Open {
    opp: Opportunity,
}

struct CurrentTx {
    block: u64,
    tx_index: u64,
    hash: B256,
    pools: Vec<Address>,
    senders: HashMap<Address, Address>,
}

pub struct Detector {
    cfg: DetectCfg,
    majors: HashSet<Address>,
    pools: HashMap<Address, PoolInfo>,
    state: HashMap<Address, Reserves>,
    by_pair: HashMap<(Address, Address), Vec<Address>>,
    open: HashMap<(Address, Address), Open>,
    pub oracle: Oracle,
    cur: Option<CurrentTx>,
    /// When false, state and prices are updated but no opportunity is opened (warm-up).
    pub recording: bool,
    last_block: u64,
}

impl Detector {
    pub fn new(cfg: DetectCfg, oracle: Oracle, majors: Vec<Address>) -> Self {
        Self {
            cfg,
            majors: majors.into_iter().collect(),
            pools: HashMap::new(),
            state: HashMap::new(),
            by_pair: HashMap::new(),
            open: HashMap::new(),
            oracle,
            cur: None,
            recording: true,
            last_block: 0,
        }
    }

    pub fn add_pool(&mut self, info: PoolInfo) {
        let key = (info.token0, info.token1);
        let list = self.by_pair.entry(key).or_default();
        if !list.contains(&info.address) {
            list.push(info.address);
        }
        self.pools.insert(info.address, info);
    }

    pub fn open_count(&self) -> usize {
        self.open.len()
    }

    /// Apply one event. Returns opportunities that finished as a result.
    pub fn on_event(&mut self, ev: &PoolEvent) -> Vec<Opportunity> {
        let mut done = Vec::new();
        let Some(fee) = self.pools.get(&ev.pool).map(|i| i.fee_ppm) else { return done };
        let same_tx = matches!(&self.cur, Some(c) if c.block == ev.block && c.tx_index == ev.tx_index);
        if !same_tx {
            done.extend(self.finish_tx());
            if ev.block != self.last_block {
                done.extend(self.expire(ev.block));
                self.last_block = ev.block;
            }
            self.cur = Some(CurrentTx {
                block: ev.block,
                tx_index: ev.tx_index,
                hash: ev.tx_hash,
                pools: Vec::new(),
                senders: HashMap::new(),
            });
        }
        let reserves = match ev.update {
            Update::Cl { sqrt_price_x96, liquidity, .. } => Reserves::from_cl(sqrt_price_x96, liquidity, fee),
            Update::V2 { r0, r1 } => Reserves::from_v2(r0, r1, fee),
        };
        match reserves {
            Some(r) => {
                self.state.insert(ev.pool, r);
            }
            None => {
                self.state.remove(&ev.pool);
            }
        }
        let cur = self.cur.as_mut().expect("set above");
        if !cur.pools.contains(&ev.pool) {
            cur.pools.push(ev.pool);
        }
        if let Update::Cl { sender, .. } = ev.update {
            cur.senders.entry(ev.pool).or_insert(sender);
        }
        done
    }

    /// Flush the pending transaction (call at the end of a batch of logs).
    pub fn finish_tx(&mut self) -> Vec<Opportunity> {
        let Some(cur) = self.cur.take() else { return Vec::new() };
        // Prices first, so this tx's own moves are valued consistently.
        for p in &cur.pools {
            if let (Some(info), Some(r)) = (self.pools.get(p), self.state.get(p)) {
                self.oracle.observe(*p, info.token0, info.token1, r);
            }
        }
        let mut done = Vec::new();
        let mut seen = HashSet::new();
        for p in &cur.pools {
            let Some(info) = self.pools.get(p) else { continue };
            let siblings = self.by_pair.get(&(info.token0, info.token1)).cloned().unwrap_or_default();
            for q in siblings {
                if q == *p {
                    continue;
                }
                let key = if *p < q { (*p, q) } else { (q, *p) };
                if !seen.insert(key) {
                    continue;
                }
                if let Some(o) = self.evaluate(key, &cur) {
                    done.push(o);
                }
            }
        }
        done
    }

    fn evaluate(&mut self, key: (Address, Address), cur: &CurrentTx) -> Option<Opportunity> {
        let (a, b) = key;
        let (ra, rb) = (self.state.get(&a).copied(), self.state.get(&b).copied());
        let gross = match (ra, rb) {
            (Some(ra), Some(rb)) => self.gross_usd(a, &ra, &rb),
            _ => None,
        };
        let close_level = self.cfg.min_gross_usd * 0.5;
        if let Some(open) = self.open.get_mut(&key) {
            match gross {
                Some(g) if g >= close_level => {
                    open.opp.peak_usd = open.opp.peak_usd.max(g);
                    None
                }
                _ => {
                    let mut opp = self.open.remove(&key).unwrap().opp;
                    let touched_both = cur.pools.contains(&a) && cur.pools.contains(&b);
                    opp.close_block = cur.block;
                    opp.close_tx = Some(cur.hash);
                    opp.close_kind = if touched_both { CloseKind::Arb } else { CloseKind::Organic };
                    opp.closer_sender = cur.senders.get(&a).or_else(|| cur.senders.get(&b)).copied();
                    Some(opp)
                }
            }
        } else {
            let g = gross?;
            if !self.recording || g < self.cfg.min_gross_usd {
                return None;
            }
            let (ra, rb) = (ra?, rb?);
            let (ia, ib) = (self.pools.get(&a)?, self.pools.get(&b)?);
            let depth_a = self.oracle.pool_depth_usd(ia.token0, ia.token1, &ra)?;
            let depth_b = self.oracle.pool_depth_usd(ib.token0, ib.token1, &rb)?;
            let min_depth = depth_a.min(depth_b);
            if min_depth < self.cfg.min_pool_liquidity_usd {
                return None;
            }
            let gap = gap_pct(&ra, &rb);
            let opp = Opportunity {
                pool_a: a,
                pool_b: b,
                venue_a: ia.venue.clone(),
                venue_b: ib.venue.clone(),
                token0: ia.token0,
                token1: ia.token1,
                major: self.majors.contains(&ia.token0) && self.majors.contains(&ia.token1),
                open_block: cur.block,
                open_tx: cur.hash,
                gross_usd: g,
                peak_usd: g,
                gap_pct: gap,
                suspect: gap > self.cfg.suspect_gap_pct,
                min_depth_usd: min_depth,
                close_block: 0,
                close_tx: None,
                close_kind: CloseKind::Stale,
                closer_sender: None,
            };
            self.open.insert(key, Open { opp });
            None
        }
    }

    fn gross_usd(&self, a: Address, ra: &Reserves, rb: &Reserves) -> Option<f64> {
        let info = self.pools.get(&a)?;
        let arb = best_arb(ra, rb).or_else(|| best_arb(rb, ra))?;
        let token = if arb.profit_token == 0 { info.token0 } else { info.token1 };
        self.oracle.usd(token, arb.profit)
    }

    fn expire(&mut self, block: u64) -> Vec<Opportunity> {
        let stale: Vec<_> = self
            .open
            .iter()
            .filter(|(_, o)| block.saturating_sub(o.opp.open_block) > STALE_BLOCKS)
            .map(|(k, _)| *k)
            .collect();
        stale
            .into_iter()
            .filter_map(|k| self.open.remove(&k))
            .map(|o| {
                let mut opp = o.opp;
                opp.close_block = block;
                opp.close_kind = CloseKind::Stale;
                opp
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(n: u8) -> Address {
        Address::repeat_byte(n)
    }
    const WETH: u8 = 1;
    const USDC: u8 = 2;
    const TOK: u8 = 3;

    fn cfg() -> DetectCfg {
        DetectCfg {
            min_gross_usd: 0.15,
            gas_cost_usd: 0.03,
            suspect_gap_pct: 30.0,
            min_pool_liquidity_usd: 1000.0,
            max_closer_tx_lookups: 10,
        }
    }

    fn det() -> Detector {
        let mut d = Detector::new(cfg(), Oracle::new(addr(WETH), vec![addr(USDC)]), vec![addr(WETH), addr(USDC)]);
        // Two TOK/USDC v2 pools (TOK 18 dec, USDC 6 dec), 0.3% fee each.
        for (p, venue) in [(10u8, "uni"), (11u8, "aero")] {
            d.add_pool(PoolInfo {
                address: addr(p),
                venue: venue.into(),
                token0: addr(TOK),
                token1: addr(USDC),
                fee_ppm: 3000,
                is_cl: false,
            });
        }
        d
    }

    fn sync(block: u64, txi: u64, pool: u8, r0: f64, r1: f64) -> PoolEvent {
        PoolEvent {
            block,
            tx_index: txi,
            log_index: 0,
            tx_hash: B256::repeat_byte((block * 10 + txi) as u8),
            pool: addr(pool),
            update: Update::V2 { r0, r1 },
        }
    }

    // 10,000 TOK vs 10,000 USDC => $1 per TOK, $20k depth.
    const BAL: (f64, f64) = (10_000e18, 10_000e6);

    #[test]
    fn opens_on_gap_and_classifies_arb_close() {
        let mut d = det();
        assert!(d.on_event(&sync(1, 0, 10, BAL.0, BAL.1)).is_empty());
        assert!(d.on_event(&sync(1, 1, 11, BAL.0, BAL.1)).is_empty());
        // A big buy on pool 11 pushes TOK to ~$1.02 there.
        d.on_event(&sync(2, 0, 11, 9_900e18, 10_101e6));
        assert!(d.finish_tx().is_empty());
        assert_eq!(d.open_count(), 1);
        // Next block, one tx touches both pools and restores parity.
        assert!(d.on_event(&sync(3, 0, 10, 9_950e18, 10_050e6)).is_empty());
        assert!(d.on_event(&sync(3, 0, 11, 9_950e18, 10_050e6)).is_empty());
        let done = d.finish_tx();
        assert_eq!(done.len(), 1);
        let o = &done[0];
        assert_eq!(o.close_kind, CloseKind::Arb);
        assert_eq!(o.delay_blocks(), 1);
        assert!(!o.same_block());
        assert!(!o.major);
        assert!(o.gross_usd > 0.15 && o.gross_usd < 50.0, "gross {}", o.gross_usd);
    }

    #[test]
    fn intra_tx_moves_are_not_opportunities() {
        let mut d = det();
        d.on_event(&sync(1, 0, 10, BAL.0, BAL.1));
        d.on_event(&sync(1, 1, 11, BAL.0, BAL.1));
        // A router moves pool 11 then pool 10 by the same amount in ONE tx.
        d.on_event(&sync(2, 0, 11, 9_900e18, 10_101e6));
        d.on_event(&sync(2, 0, 10, 9_900e18, 10_101e6));
        d.finish_tx();
        assert_eq!(d.open_count(), 0);
    }

    #[test]
    fn organic_close_and_same_block() {
        let mut d = det();
        d.on_event(&sync(1, 0, 10, BAL.0, BAL.1));
        d.on_event(&sync(1, 1, 11, BAL.0, BAL.1));
        d.on_event(&sync(2, 0, 11, 9_900e18, 10_101e6));
        // Same block, a seller pushes pool 11 back: organic, same block.
        let done = d.on_event(&sync(2, 1, 11, BAL.0, BAL.1));
        let mut done = done;
        done.extend(d.finish_tx());
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].close_kind, CloseKind::Organic);
        assert!(done[0].same_block());
    }

    #[test]
    fn warmup_records_nothing_and_stale_expires() {
        let mut d = det();
        d.recording = false;
        d.on_event(&sync(1, 0, 10, BAL.0, BAL.1));
        d.on_event(&sync(1, 1, 11, 9_900e18, 10_101e6));
        d.finish_tx();
        assert_eq!(d.open_count(), 0);

        d.recording = true;
        d.on_event(&sync(2, 0, 11, 9_890e18, 10_111e6));
        d.finish_tx();
        assert_eq!(d.open_count(), 1);
        let done = d.on_event(&sync(2 + STALE_BLOCKS + 1, 0, 10, BAL.0, BAL.1));
        assert!(done.iter().any(|o| o.close_kind == CloseKind::Stale));
    }

    #[test]
    fn shallow_pools_are_ignored() {
        let mut d = det();
        d.on_event(&sync(1, 0, 10, 100e18, 100e6)); // $200 depth
        d.on_event(&sync(1, 1, 11, 90e18, 111e6));
        d.finish_tx();
        assert_eq!(d.open_count(), 0);
    }

    #[test]
    fn unknown_pools_are_ignored() {
        let mut d = det();
        assert!(d.on_event(&sync(1, 0, 99, BAL.0, BAL.1)).is_empty());
        assert!(d.finish_tx().is_empty());
    }
}
