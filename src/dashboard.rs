//! JSON summary for the web dashboard. Read-only queries over the ledger.

use crate::config::Config;
use crate::report::{collect, gate, quantile};
use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;

/// Days are cut at midnight Taipei time (UTC+8).
const TZ_OFFSET: &str = "+8 hours";
/// The plan's first decision point.
/// Leftovers show up in volatile markets, which may take weeks to arrive.
pub const DECISION_DAY: f64 = 30.0;

#[derive(Debug, Serialize)]
pub struct Summary {
    pub chain: crate::config::ChainCfg,
    pub verdict: Verdict,
    pub metrics: Vec<Metric>,
    pub health: Health,
    pub endings: Vec<Ending>,
    pub daily: Vec<Day>,
    pub recent: Vec<Recent>,
    pub closers: Vec<Closer>,
    pub alpha: Option<f64>,
    pub crowding: Crowding,
}

/// Tests "chains where failing costs gas are less crowded".
#[derive(Debug, Serialize, Default)]
pub struct Crowding {
    /// Blocks sampled (ones where a rival arb landed).
    pub blocks: i64,
    /// Reverted contract calls ÷ contract calls in those blocks.
    pub reverted_call_share: Option<f64>,
    /// Gas burned by reverted txs ÷ all gas in those blocks.
    pub reverted_gas_share: Option<f64>,
    /// Reverted calls per sampled block.
    pub reverted_per_block: Option<f64>,
    /// Distinct contracts that took opportunities.
    pub rivals: i64,
    /// Share taken by the biggest one.
    pub top_rival_share: Option<f64>,
    /// What one failed attempt costs us here.
    pub fail_cost_usd: f64,
    /// Median net profit per pickup ÷ fail cost: how many misses one win can pay for.
    pub misses_per_win: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct Verdict {
    /// "pass" | "fail" | "insufficient"
    pub state: &'static str,
    pub reasons: Vec<String>,
    pub hours: f64,
    pub days_left: f64,
}

#[derive(Debug, Serialize)]
pub struct Metric {
    pub key: &'static str,
    pub label: &'static str,
    pub value: f64,
    pub threshold: f64,
    /// true when higher is better
    pub higher_is_better: bool,
    pub unit: &'static str,
    pub ok: bool,
    /// false: shown for context only, does not decide the verdict.
    pub gating: bool,
}

#[derive(Debug, Serialize, Default)]
pub struct Health {
    pub last_block: u64,
    pub last_block_ts: u64,
    pub chain_head: u64,
    pub lag_blocks: u64,
    pub lag_seconds: i64,
    pub recording: bool,
    pub rpc_errors: u64,
    pub pools_verified: i64,
    pub pools_rejected: i64,
    pub opportunities: i64,
}

#[derive(Debug, Serialize)]
pub struct Ending {
    pub key: &'static str,
    pub label: &'static str,
    pub count: i64,
    pub gross_usd: f64,
}

#[derive(Debug, Serialize)]
pub struct Day {
    pub date: String,
    pub hours: f64,
    pub count: i64,
    pub median_gross: f64,
    pub addressable_usd: f64,
    pub same_block_share: f64,
}

#[derive(Debug, Serialize)]
pub struct Recent {
    pub ts: i64,
    pub token0: String,
    pub token1: String,
    pub venues: String,
    pub gross_usd: f64,
    pub kind: String,
    pub delay_blocks: i64,
    pub closer: Option<String>,
    pub close_tx: Option<String>,
    pub suspect: bool,
    pub major: bool,
}

#[derive(Debug, Serialize)]
pub struct Closer {
    pub address: String,
    pub count: i64,
    pub gross_usd: f64,
    pub share: f64,
}

const ENDING_SQL: &str = "CASE
    WHEN suspect=1 THEN 'suspect'
    WHEN close_kind='stale' THEN 'stale'
    WHEN close_kind='organic' THEN 'organic'
    WHEN same_block=1 THEN 'arb_same'
    WHEN delay_blocks=1 THEN 'arb_next'
    ELSE 'arb_later' END";

fn ending_label(k: &str) -> &'static str {
    match k {
        "arb_same" => "套利者・同一區塊",
        "arb_next" => "套利者・下一區塊",
        "arb_later" => "套利者・更晚",
        "organic" => "一般交易抹平",
        "stale" => "過期沒人動",
        _ => "可疑（價差過大）",
    }
}

fn ending_key(k: &str) -> &'static str {
    match k {
        "arb_same" => "arb_same",
        "arb_next" => "arb_next",
        "arb_later" => "arb_later",
        "organic" => "organic",
        "stale" => "stale",
        _ => "suspect",
    }
}

pub struct Live {
    pub chain_head: u64,
    pub last_ts: u64,
    pub recording: bool,
    pub rpc_errors: u64,
    pub now: i64,
}

pub fn build(c: &Connection, cfg: &Config, live: &Live) -> Result<Summary> {
    let s = collect(c, cfg)?;
    let v = gate(&s, cfg);
    let g = &cfg.gate;
    let gas = cfg.detect.gas_cost_usd;
    let median = quantile(&s.clean, 0.5);
    let per_day = s.per_day(s.addressable_usd);
    let share = s.same_block_share();
    let state = if s.hours < 24.0 {
        "insufficient"
    } else if v.pass {
        "pass"
    } else {
        "fail"
    };
    let metrics = vec![
        Metric {
            key: "median",
            label: "毛利中位數",
            value: median,
            threshold: g.min_median_gross_usd,
            higher_is_better: true,
            unit: "usd",
            ok: median >= g.min_median_gross_usd,
            gating: false,
        },
        Metric {
            key: "addressable",
            label: "每天可爭取淨利",
            value: per_day,
            threshold: g.min_addressable_usd_per_day,
            higher_is_better: true,
            unit: "usd",
            ok: per_day >= g.min_addressable_usd_per_day,
            gating: true,
        },
        Metric {
            key: "same_block",
            label: "同區塊被吃比例",
            value: share,
            threshold: g.max_same_block_share,
            higher_is_better: false,
            unit: "pct",
            ok: share <= g.max_same_block_share,
            gating: false,
        },
    ];

    let last_block: u64 = c
        .query_row("SELECT value FROM meta WHERE key='last_block'", [], |r| r.get::<_, String>(0))
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let last_ts = if live.last_ts > 0 {
        live.last_ts
    } else {
        c.query_row("SELECT COALESCE(MAX(to_ts),0) FROM coverage", [], |r| r.get::<_, i64>(0))? as u64
    };
    let health = Health {
        last_block,
        last_block_ts: last_ts,
        chain_head: live.chain_head,
        lag_blocks: live.chain_head.saturating_sub(last_block),
        lag_seconds: if last_ts > 0 { live.now - last_ts as i64 } else { 0 },
        recording: live.recording,
        rpc_errors: live.rpc_errors,
        pools_verified: s.pools_verified,
        pools_rejected: s.pools_rejected,
        opportunities: c.query_row("SELECT COUNT(*) FROM opportunities", [], |r| r.get(0))?,
    };

    let mut endings = Vec::new();
    {
        let mut st = c.prepare(&format!(
            "SELECT {ENDING_SQL} AS k, COUNT(*), COALESCE(SUM(gross_usd),0) FROM opportunities GROUP BY k"
        ))?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, f64>(2)?)))?;
        let mut got: Vec<(String, i64, f64)> = rows.collect::<Result<_, _>>()?;
        let order = ["arb_same", "arb_next", "arb_later", "organic", "stale", "suspect"];
        got.sort_by_key(|(k, _, _)| order.iter().position(|o| o == k).unwrap_or(99));
        for (k, n, gsum) in got {
            endings.push(Ending { key: ending_key(&k), label: ending_label(&k), count: n, gross_usd: gsum });
        }
    }

    // Daily series.
    let mut daily = Vec::new();
    {
        let mut hours_st = c.prepare(&format!(
            "SELECT date(from_ts,'unixepoch','{TZ_OFFSET}') d, SUM(to_ts-from_ts)/3600.0
             FROM coverage WHERE recording=1 GROUP BY d ORDER BY d"
        ))?;
        let days: Vec<(String, f64)> =
            hours_st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
        let mut gross_st = c.prepare(&format!(
            "SELECT gross_usd FROM opportunities
             WHERE suspect=0 AND close_kind!='stale' AND date(open_ts,'unixepoch','{TZ_OFFSET}')=?1 ORDER BY gross_usd"
        ))?;
        let mut agg_st = c.prepare(&format!(
            "SELECT
               COALESCE(SUM(CASE WHEN close_kind='arb' AND same_block=0 AND gross_usd>?2 THEN gross_usd-?2 ELSE 0 END),0),
               SUM(CASE WHEN close_kind='arb' THEN 1 ELSE 0 END),
               SUM(CASE WHEN close_kind='arb' AND same_block=1 THEN 1 ELSE 0 END)
             FROM opportunities WHERE suspect=0 AND date(open_ts,'unixepoch','{TZ_OFFSET}')=?1"
        ))?;
        for (date, hours) in days {
            let gross: Vec<f64> = gross_st.query_map([&date], |r| r.get(0))?.collect::<Result<_, _>>()?;
            let (addr, arb, same): (f64, Option<i64>, Option<i64>) =
                agg_st.query_row(rusqlite::params![date, gas], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            let (arb, same) = (arb.unwrap_or(0), same.unwrap_or(0));
            let scale = if hours > 0.0 { 24.0 / hours } else { 0.0 };
            daily.push(Day {
                date,
                hours,
                count: gross.len() as i64,
                median_gross: quantile(&gross, 0.5),
                addressable_usd: addr * scale,
                same_block_share: if arb > 0 { same as f64 / arb as f64 } else { 0.0 },
            });
        }
    }

    let recent: Vec<Recent>;
    {
        let mut st = c.prepare(
            "SELECT open_ts, token0, token1, venue_a, venue_b, gross_usd, close_kind, same_block, delay_blocks,
                    closer_sender, close_tx, suspect, major
             FROM opportunities ORDER BY id DESC LIMIT 20",
        )?;
        let rows = st.query_map([], |r| {
            let (va, vb): (String, String) = (r.get(3)?, r.get(4)?);
            let kind: String = r.get(6)?;
            let same: i64 = r.get(7)?;
            let suspect: i64 = r.get(11)?;
            let k = if suspect == 1 {
                "suspect"
            } else if kind == "arb" && same == 1 {
                "arb_same"
            } else if kind == "arb" {
                "arb"
            } else {
                ending_key(&kind)
            };
            Ok(Recent {
                ts: r.get(0)?,
                token0: r.get(1)?,
                token1: r.get(2)?,
                venues: if va <= vb { format!("{va} × {vb}") } else { format!("{vb} × {va}") },
                gross_usd: r.get(5)?,
                kind: k.to_string(),
                delay_blocks: r.get(8)?,
                closer: r.get(9)?,
                close_tx: r.get(10)?,
                suspect: suspect == 1,
                major: r.get::<_, i64>(12)? == 1,
            })
        })?;
        recent = rows.collect::<Result<_, _>>()?;
    }

    let mut closers = Vec::new();
    {
        let mut st = c.prepare(
            "SELECT closer_sender, COUNT(*), SUM(gross_usd) FROM opportunities
             WHERE close_kind='arb' AND suspect=0 AND closer_sender IS NOT NULL
             GROUP BY closer_sender ORDER BY COUNT(*) DESC LIMIT 10",
        )?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, f64>(2)?)))?;
        for row in rows {
            let (address, count, gross_usd) = row?;
            let share = if s.arb_total > 0 { count as f64 / s.arb_total as f64 } else { 0.0 };
            closers.push(Closer { address, count, gross_usd, share });
        }
    }

    let alpha: Option<f64> = c.query_row(
        "SELECT AVG(CASE WHEN o.gross_usd>0 THEN t.priority_usd/o.gross_usd END)
         FROM closer_txs t JOIN opportunities o ON o.id=t.opp_id",
        [],
        |r| r.get(0),
    )?;

    let crowding = crowding(c, cfg, &s)?;

    Ok(Summary {
        chain: cfg.chain.clone(),
        verdict: Verdict {
            state,
            reasons: v.reasons,
            hours: s.hours,
            days_left: (DECISION_DAY - s.hours / 24.0).max(0.0),
        },
        metrics,
        health,
        endings,
        daily,
        recent,
        closers,
        alpha,
        crowding,
    })
}

fn crowding(c: &Connection, cfg: &Config, s: &crate::report::Stats) -> Result<Crowding> {
    let (blocks, calls, reverted, block_gas, rgas): (i64, i64, i64, i64, i64) = c.query_row(
        "SELECT COUNT(*), COALESCE(SUM(calls),0), COALESCE(SUM(reverted),0), COALESCE(SUM(gas),0), COALESCE(SUM(reverted_gas),0)
         FROM block_stats",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    let ratio = |a: i64, b: i64| (b > 0).then(|| a as f64 / b as f64);
    let (rivals, top): (i64, Option<i64>) = c.query_row(
        "SELECT COUNT(*), MAX(n) FROM (SELECT COUNT(*) n FROM opportunities
         WHERE close_kind='arb' AND suspect=0 AND closer_sender IS NOT NULL GROUP BY closer_sender)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let fail = cfg.detect.fail_cost_usd;
    let gas_usd = cfg.detect.gas_cost_usd;
    let mut st = c.prepare(
        "SELECT gross_usd - ?1 FROM opportunities
         WHERE suspect=0 AND close_kind='arb' AND same_block=0 AND gross_usd > ?1 ORDER BY 1",
    )?;
    let nets: Vec<f64> = st.query_map([gas_usd], |r| r.get(0))?.collect::<Result<_, _>>()?;
    let net = (!nets.is_empty()).then(|| quantile(&nets, 0.5));
    Ok(Crowding {
        blocks,
        reverted_call_share: ratio(reverted, calls),
        reverted_gas_share: ratio(rgas, block_gas),
        reverted_per_block: ratio(reverted, blocks),
        rivals,
        top_rival_share: top.and_then(|t| ratio(t, s.arb_total as i64)),
        fail_cost_usd: fail,
        misses_per_win: (fail > 0.0).then_some(()).and(net).map(|n| n / fail),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detector::{CloseKind, Opportunity};
    use crate::ledger::Ledger;
    use alloy::primitives::{Address, B256};
    use std::path::Path;

    #[test]
    fn builds_summary_with_daily_series() {
        let cfg = Config::load(Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/config.toml"))).unwrap();
        let l = Ledger::in_memory().unwrap();
        // Two days of coverage starting 2026-11-20 00:00 Taipei (= 2026-11-19 16:00 UTC).
        let t0: u64 = 1795104000;
        l.add_coverage(1, 345_600, t0, t0 + 86_400, true, 0).unwrap();
        l.add_coverage(345_601, 691_200, t0 + 86_400, t0 + 172_800, true, 0).unwrap();
        for i in 0..20u64 {
            let o = Opportunity {
                pool_a: Address::repeat_byte(1),
                pool_b: Address::repeat_byte(2),
                venue_a: "uniswap_v3".into(),
                venue_b: "aero_cl".into(),
                token0: Address::repeat_byte(3),
                token1: Address::repeat_byte(4),
                major: false,
                open_block: i,
                open_tx: B256::ZERO,
                gross_usd: 0.5 + i as f64 * 0.1,
                peak_usd: 1.0,
                gap_pct: 1.0,
                suspect: false,
                min_depth_usd: 10_000.0,
                close_block: if i % 2 == 0 { i } else { i + 1 },
                close_tx: Some(B256::ZERO),
                close_kind: CloseKind::Arb,
                closer_sender: Some(Address::repeat_byte(9)),
            };
            let ts = t0 + if i < 10 { 3_600 } else { 90_000 };
            l.insert_opportunity(&o, 0, ts as i64).unwrap();
        }
        let live = Live { chain_head: 700_000, last_ts: 0, recording: true, rpc_errors: 3, now: (t0 + 172_900) as i64 };
        let s = build(&l.conn, &cfg, &live).unwrap();
        assert_eq!(s.daily.len(), 2);
        assert_eq!(s.daily[0].date, "2026-11-20");
        assert_eq!(s.daily[0].count, 10);
        assert!((s.daily[0].same_block_share - 0.5).abs() < 1e-9);
        assert_eq!(s.metrics.len(), 3);
        assert_eq!(s.health.rpc_errors, 3);
        assert_eq!(s.closers.len(), 1);
        assert!((s.closers[0].share - 1.0).abs() < 1e-9);
        assert_eq!(s.recent.len(), 20); // capped at 20
        assert!(s.endings.iter().any(|e| e.key == "arb_same"));
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"state\""));
    }
}
