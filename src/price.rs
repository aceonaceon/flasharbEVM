//! USD oracle built only from the pool states we already track.
//!
//! Stablecoins are $1. WETH is priced from its deepest stable pool. Every other
//! token is priced from its deepest pool against WETH or a stable. Prices are
//! per *raw* unit, so token decimals are never needed except for the stables
//! themselves: 6 by default, 18 for those listed in `tokens.stables18`.

use crate::amm::Reserves;
use alloy::primitives::Address;
use std::collections::HashMap;

pub const STABLE_DECIMALS: i32 = 6;

#[derive(Debug, Clone, Copy)]
struct Quote {
    usd_per_raw: f64,
    /// USD depth of the anchor side of the source pool; deeper sources win.
    depth_usd: f64,
    source: Address,
}

#[derive(Debug, Clone)]
pub struct Oracle {
    weth: Address,
    stables: Vec<Address>,
    stables18: Vec<Address>,
    quotes: HashMap<Address, Quote>,
}

impl Oracle {
    pub fn new(weth: Address, stables: Vec<Address>) -> Self {
        Self { weth, stables, stables18: Vec::new(), quotes: HashMap::new() }
    }

    /// Add stablecoins that use 18 decimals.
    pub fn with_stables18(mut self, v: Vec<Address>) -> Self {
        self.stables.extend(v.iter().copied());
        self.stables18 = v;
        self
    }

    pub fn is_stable(&self, t: Address) -> bool {
        self.stables.contains(&t)
    }

    pub fn usd_per_raw(&self, t: Address) -> Option<f64> {
        if self.is_stable(t) {
            let dec = if self.stables18.contains(&t) { 18 } else { STABLE_DECIMALS };
            return Some(10f64.powi(-dec));
        }
        self.quotes.get(&t).map(|q| q.usd_per_raw)
    }

    /// USD value of `raw` units of token `t`.
    pub fn usd(&self, t: Address, raw: f64) -> Option<f64> {
        self.usd_per_raw(t).map(|p| p * raw)
    }

    /// USD depth of a pool: twice the value of its priced side (the smaller one if both are priced).
    pub fn pool_depth_usd(&self, t0: Address, t1: Address, r: &Reserves) -> Option<f64> {
        match (self.usd(t0, r.r0), self.usd(t1, r.r1)) {
            (Some(a), Some(b)) => Some(2.0 * a.min(b)),
            (Some(a), None) => Some(2.0 * a),
            (None, Some(b)) => Some(2.0 * b),
            (None, None) => None,
        }
    }

    /// Feed a fresh pool state. Updates the quote of a non-anchor token when this
    /// pool pairs it with an anchor (stable, or WETH once WETH is priced).
    pub fn observe(&mut self, pool: Address, t0: Address, t1: Address, r: &Reserves) {
        // Try both orientations: (anchor, other).
        for (anchor, a_res, other, o_res) in [(t0, r.r0, t1, r.r1), (t1, r.r1, t0, r.r0)] {
            if self.is_stable(other) {
                continue; // stables are fixed at $1
            }
            let anchor_ok = self.is_stable(anchor) || anchor == self.weth;
            if !anchor_ok || other == anchor {
                continue;
            }
            // WETH may only be priced from stables, so a long-tail token can't move WETH.
            if other == self.weth && !self.is_stable(anchor) {
                continue;
            }
            let Some(a_px) = self.usd_per_raw(anchor) else { continue };
            if !(o_res > 0.0) {
                continue;
            }
            let depth = a_res * a_px;
            let usd_per_raw = a_res / o_res * a_px;
            let replace = match self.quotes.get(&other) {
                None => true,
                Some(q) => q.source == pool || depth >= q.depth_usd,
            };
            if replace && usd_per_raw.is_finite() {
                self.quotes.insert(other, Quote { usd_per_raw, depth_usd: depth, source: pool });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(n: u8) -> Address {
        Address::repeat_byte(n)
    }

    #[test]
    fn prices_weth_then_longtail() {
        let (weth, usdc, tok) = (a(1), a(2), a(3));
        let mut o = Oracle::new(weth, vec![usdc]);
        // WETH (18 dec) / USDC (6 dec): 100 WETH vs 300,000 USDC -> $3000 per WETH.
        let r = Reserves::from_v2(100e18, 300_000e6, 3000).unwrap();
        o.observe(a(10), weth, usdc, &r);
        let weth_usd = o.usd(weth, 1e18).unwrap();
        assert!((weth_usd - 3000.0).abs() < 1e-6);
        // TOK / WETH: 1,000,000 TOK vs 10 WETH -> 1 TOK = 0.00001 WETH = $0.03.
        let r2 = Reserves::from_v2(1_000_000e18, 10e18, 3000).unwrap();
        o.observe(a(11), tok, weth, &r2);
        assert!((o.usd(tok, 1e18).unwrap() - 0.03).abs() < 1e-9);
        // Depth: $30k on the WETH side -> $60k.
        assert!((o.pool_depth_usd(tok, weth, &r2).unwrap() - 60_000.0).abs() < 1e-6);
    }

    #[test]
    fn shallow_pool_cannot_override_deep_quote() {
        let (weth, usdc, tok) = (a(1), a(2), a(3));
        let mut o = Oracle::new(weth, vec![usdc]);
        o.observe(a(10), tok, usdc, &Reserves::from_v2(1e6, 1e6, 3000).unwrap()); // $1, depth $1
        o.observe(a(11), tok, usdc, &Reserves::from_v2(1e3, 5e3, 3000).unwrap()); // $5, depth $0.005
        assert!((o.usd_per_raw(tok).unwrap() - 1e-6).abs() < 1e-12);
    }

    #[test]
    fn longtail_cannot_price_weth() {
        let (weth, usdc, tok) = (a(1), a(2), a(3));
        let mut o = Oracle::new(weth, vec![usdc]);
        o.observe(a(10), tok, weth, &Reserves::from_v2(1e18, 1e18, 3000).unwrap());
        assert!(o.usd_per_raw(weth).is_none());
        assert!(o.usd_per_raw(tok).is_none());
    }
}
