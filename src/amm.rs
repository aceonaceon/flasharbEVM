//! Pool math used by the detector.
//!
//! Every pool is reduced to "virtual reserves" (x, y) in raw token units:
//! - v2 pools: the actual reserves from the Sync event.
//! - concentrated-liquidity pools: x = L / sqrtP, y = L * sqrtP, which is exact
//!   only while the price stays inside the current tick-spacing bucket. Outside
//!   it L is unknown (often far smaller), so each CL pool also carries the most
//!   it can absorb before leaving the bucket, and trades are capped there. That
//!   makes CL profits a lower bound instead of a (sometimes wild) overestimate.
//!
//! Two constant-product legs compose into a single Möbius map, which gives the
//! optimal input and profit in closed form (no numeric search on the hot path).

/// Virtual reserves plus the fraction of input that survives the fee (gamma = 1 - fee).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reserves {
    pub r0: f64,
    pub r1: f64,
    pub gamma: f64,
    /// Largest token0 input (before fee) the pool is known to absorb at this L.
    pub cap_in0: f64,
    /// Largest token1 input (before fee) the pool is known to absorb at this L.
    pub cap_in1: f64,
}

impl Reserves {
    /// `tick_spacing` <= 0 means unknown: assume a single tick (most conservative).
    pub fn from_cl(sqrt_price_x96: f64, liquidity: f64, fee_ppm: u32, tick_spacing: i32) -> Option<Self> {
        if !(sqrt_price_x96 > 0.0) || !(liquidity > 0.0) {
            return None;
        }
        let sqrt_p = sqrt_price_x96 / 2f64.powi(96);
        let g = gamma(fee_ppm);
        let ts = tick_spacing.max(1) as f64;
        // Bucket [lo, lo + ts) containing the current tick; sqrt(1.0001^t) = 1.0001^(t/2).
        let tick = (sqrt_p * sqrt_p).ln() / 1.0001f64.ln();
        let lo = (tick / ts).floor() * ts;
        let sqrt_lo = 1.0001f64.powf(lo / 2.0);
        let sqrt_hi = 1.0001f64.powf((lo + ts) / 2.0);
        // token1 in pushes the price up to the top edge; token0 in pushes it down to the bottom edge.
        let cap_in1 = (liquidity * (sqrt_hi - sqrt_p)).max(0.0) / g;
        let cap_in0 = (liquidity * (1.0 / sqrt_lo - 1.0 / sqrt_p)).max(0.0) / g;
        Some(Self { r0: liquidity / sqrt_p, r1: liquidity * sqrt_p, gamma: g, cap_in0, cap_in1 })
    }

    pub fn from_v2(r0: f64, r1: f64, fee_ppm: u32) -> Option<Self> {
        if !(r0 > 0.0) || !(r1 > 0.0) {
            return None;
        }
        Some(Self { r0, r1, gamma: gamma(fee_ppm), cap_in0: f64::INFINITY, cap_in1: f64::INFINITY })
    }

    /// Price of token0 in token1 raw units.
    pub fn price(&self) -> f64 {
        self.r1 / self.r0
    }

    #[cfg(test)]
    /// Output for `dx` of input on a constant-product curve.
    pub fn out(&self, input_reserve: f64, output_reserve: f64, dx: f64) -> f64 {
        let x = self.gamma * dx;
        output_reserve * x / (input_reserve + x)
    }
}

pub fn gamma(fee_ppm: u32) -> f64 {
    1.0 - fee_ppm as f64 / 1_000_000.0
}

/// Which token the arbitrage starts and ends in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    /// token1 -> P -> token0 -> Q -> token1 (buy token0 on P, sell on Q); profit in token1.
    BuyOnPSellOnQProfit1,
    /// token0 -> P -> token1 -> Q -> token0 (sell token0 on P... i.e. buy token1 on P); profit in token0.
    BuyOnPSellOnQProfit0,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Arb {
    pub dir: Dir,
    pub amount_in: f64,
    /// Profit in raw units of the start token.
    pub profit: f64,
    /// 0 or 1: the token the profit is denominated in.
    pub profit_token: u8,
}

/// Closed-form optimum for two chained constant-product legs.
/// Leg 1: input reserve `y1`, output reserve `x1`, fee factor `g1`.
/// Leg 2: input reserve `x2`, output reserve `y2`, fee factor `g2`.
/// out(d) = a d / (b + c d) with a = g1 g2 x1 y2, b = x2 y1, c = g1 (x2 + g2 x1).
/// Profit out(d) - d peaks at d* = (sqrt(ab) - b) / c with value (sqrt(a) - sqrt(b))^2 / c.
pub fn two_leg_optimum(y1: f64, x1: f64, g1: f64, x2: f64, y2: f64, g2: f64) -> Option<(f64, f64)> {
    let a = g1 * g2 * x1 * y2;
    let b = x2 * y1;
    let c = g1 * (x2 + g2 * x1);
    if !(a > b) || !(c > 0.0) {
        return None;
    }
    let d = ((a * b).sqrt() - b) / c;
    let profit = (a.sqrt() - b.sqrt()).powi(2) / c;
    if d > 0.0 && profit > 0.0 && profit.is_finite() {
        Some((d, profit))
    } else {
        None
    }
}

/// The two-leg optimum, with the input limited so neither leg leaves the range
/// where its reserves are known (`cap1` on leg 1's input, `cap2` on leg 2's input).
/// Profit is concave in the input, so the capped point is the best feasible one.
#[allow(clippy::too_many_arguments)]
pub fn two_leg_capped(
    y1: f64, x1: f64, g1: f64, cap1: f64,
    x2: f64, y2: f64, g2: f64, cap2: f64,
) -> Option<(f64, f64)> {
    let (d_opt, p_opt) = two_leg_optimum(y1, x1, g1, x2, y2, g2)?;
    // Leg 1 output o = x1 g1 d / (y1 + g1 d); inverting at o = cap2 bounds d.
    let d_cap2 = if cap2 >= x1 { f64::INFINITY } else { y1 * cap2 / (g1 * (x1 - cap2)) };
    let d_max = cap1.min(d_cap2);
    if d_opt <= d_max {
        return Some((d_opt, p_opt));
    }
    let d = d_max;
    let o1 = x1 * g1 * d / (y1 + g1 * d);
    let o2 = y2 * g2 * o1 / (x2 + g2 * o1);
    let profit = o2 - d;
    (d > 0.0 && profit > 0.0 && profit.is_finite()).then_some((d, profit))
}

/// Best arbitrage between two pools of the same pair, if any.
pub fn best_arb(p: &Reserves, q: &Reserves) -> Option<Arb> {
    // Profit in token1: token1 into P (input r1p, output r0p), token0 into Q (input r0q, output r1q).
    let a1 = two_leg_capped(p.r1, p.r0, p.gamma, p.cap_in1, q.r0, q.r1, q.gamma, q.cap_in0).map(|(d, pr)| Arb {
        dir: Dir::BuyOnPSellOnQProfit1,
        amount_in: d,
        profit: pr,
        profit_token: 1,
    });
    // Profit in token0: token0 into P (input r0p, output r1p), token1 into Q (input r1q, output r0q).
    let a0 = two_leg_capped(p.r0, p.r1, p.gamma, p.cap_in0, q.r1, q.r0, q.gamma, q.cap_in1).map(|(d, pr)| Arb {
        dir: Dir::BuyOnPSellOnQProfit0,
        amount_in: d,
        profit: pr,
        profit_token: 0,
    });
    // At most one direction can be profitable when fees are non-negative, but be safe.
    match (a1, a0) {
        (Some(x), Some(y)) => Some(if x.profit >= y.profit { x } else { y }),
        (x, y) => x.or(y),
    }
}

/// Relative price gap between two pools, in percent.
pub fn gap_pct(p: &Reserves, q: &Reserves) -> f64 {
    let (a, b) = (p.price(), q.price());
    ((a / b) - 1.0).abs() * 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brute(p: &Reserves, q: &Reserves, profit_token: u8) -> f64 {
        // Scan the input amount on a log grid and take the best simulated profit.
        let mut best = 0.0f64;
        let cap = if profit_token == 1 { p.r1 } else { p.r0 };
        let mut d = cap * 1e-9;
        while d < cap {
            let profit = if profit_token == 1 {
                let got0 = p.out(p.r1, p.r0, d);
                q.out(q.r0, q.r1, got0) - d
            } else {
                let got1 = p.out(p.r0, p.r1, d);
                q.out(q.r1, q.r0, got1) - d
            };
            best = best.max(profit);
            d *= 1.0005;
        }
        best
    }

    #[test]
    fn no_arb_when_prices_equal() {
        let p = Reserves::from_v2(1e6, 2e6, 3000).unwrap();
        let q = Reserves::from_v2(5e6, 1e7, 500).unwrap();
        assert!(best_arb(&p, &q).is_none());
    }

    #[test]
    fn no_arb_inside_fee_band() {
        // 0.5% gap but 0.3% + 0.3% fees.
        let p = Reserves::from_v2(1e6, 1e6, 3000).unwrap();
        let q = Reserves::from_v2(1e6, 1.005e6, 3000).unwrap();
        assert!(best_arb(&p, &q).is_none());
    }

    #[test]
    fn closed_form_matches_brute_force_both_directions() {
        // token0 cheap on P (price 1.0) vs Q (price 1.05): buy token0 on P, profit in token1.
        let p = Reserves::from_v2(1e6, 1e6, 3000).unwrap();
        let q = Reserves::from_v2(2e6, 2.1e6, 500).unwrap();
        let arb = best_arb(&p, &q).unwrap();
        assert_eq!(arb.profit_token, 1);
        let b = brute(&p, &q, 1);
        assert!((arb.profit - b).abs() / b < 1e-3, "closed {} brute {}", arb.profit, b);

        // Reverse: token0 expensive on P.
        let arb2 = best_arb(&q, &p).unwrap();
        assert_eq!(arb2.profit_token, 0);
        let b2 = brute(&q, &p, 0);
        assert!((arb2.profit - b2).abs() / b2 < 1e-3, "closed {} brute {}", arb2.profit, b2);
    }

    #[test]
    fn cl_virtual_reserves_reproduce_price() {
        // sqrtPriceX96 for price 4.0 (token1 per token0) is 2 * 2^96.
        let s = 2.0 * 2f64.powi(96);
        let r = Reserves::from_cl(s, 1e18, 500, 10).unwrap();
        assert!((r.price() - 4.0).abs() < 1e-9);
        assert!((r.r0 * r.r1 - 1e36).abs() / 1e36 < 1e-9); // x*y = L^2
    }

    #[test]
    fn cl_caps_stop_at_bucket_edge() {
        let s = 2.0 * 2f64.powi(96); // price 4.0, tick ~13863
        let l = 1e18;
        let r = Reserves::from_cl(s, l, 0, 60).unwrap();
        // Pushing token1 in by exactly the cap must land on the bucket's top edge (a multiple of 60).
        let sqrt_after = 2.0 + r.cap_in1 / l;
        let tick_after = (sqrt_after * sqrt_after).ln() / 1.0001f64.ln();
        assert!((tick_after / 60.0 - (tick_after / 60.0).round()).abs() < 1e-6, "tick {tick_after}");
        assert!(r.cap_in0 > 0.0 && r.cap_in1 > 0.0);
        // Wider spacing means more room.
        let wide = Reserves::from_cl(s, l, 0, 200).unwrap();
        assert!(wide.cap_in0 + wide.cap_in1 > r.cap_in0 + r.cap_in1);
    }

    #[test]
    fn capped_profit_is_never_above_uncapped() {
        let s = 2.0 * 2f64.powi(96);
        let p = Reserves::from_cl(s, 1e20, 500, 1).unwrap();
        let q = Reserves::from_cl(s * 1.02, 1e20, 500, 1).unwrap(); // ~4% price gap
        let capped = best_arb(&p, &q).unwrap();
        let mut p_inf = p;
        let mut q_inf = q;
        for r in [&mut p_inf, &mut q_inf] {
            r.cap_in0 = f64::INFINITY;
            r.cap_in1 = f64::INFINITY;
        }
        let free = best_arb(&p_inf, &q_inf).unwrap();
        assert!(capped.profit > 0.0);
        assert!(capped.profit < free.profit * 0.1, "capped {} free {}", capped.profit, free.profit);
    }

    #[test]
    fn rejects_degenerate_state() {
        assert!(Reserves::from_cl(0.0, 1e18, 500, 10).is_none());
        assert!(Reserves::from_cl(1e20, 0.0, 500, 10).is_none());
        assert!(Reserves::from_v2(0.0, 1.0, 3000).is_none());
    }
}
