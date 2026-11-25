//! Markdown report built from the ledger, with the go / no-go gate.

use crate::config::Config;
use anyhow::Result;
use rusqlite::Connection;
use std::fmt::Write;

#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub hours: f64,
    pub blocks: i64,
    pub pools_verified: i64,
    pub pools_rejected: i64,
    pub clean: Vec<f64>,
    pub arb_total: i64,
    pub arb_same_block: i64,
    pub addressable_usd: f64,
}

impl Stats {
    pub fn per_day(&self, x: f64) -> f64 {
        if self.hours > 0.0 {
            x / self.hours * 24.0
        } else {
            0.0
        }
    }
    pub fn same_block_share(&self) -> f64 {
        if self.arb_total > 0 {
            self.arb_same_block as f64 / self.arb_total as f64
        } else {
            0.0
        }
    }
}

pub fn quantile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let pos = q * (sorted.len() - 1) as f64;
    let (lo, hi) = (pos.floor() as usize, pos.ceil() as usize);
    sorted[lo] + (sorted[hi] - sorted[lo]) * (pos - lo as f64)
}

fn one<T: rusqlite::types::FromSql>(c: &Connection, sql: &str) -> Result<T> {
    Ok(c.query_row(sql, [], |r| r.get(0))?)
}

pub fn collect(c: &Connection, cfg: &Config) -> Result<Stats> {
    let gas = cfg.detect.gas_cost_usd;
    let mut s = Stats {
        hours: one::<f64>(c, "SELECT COALESCE(SUM(to_ts - from_ts),0) / 3600.0 FROM coverage WHERE recording=1")?,
        blocks: one(c, "SELECT COALESCE(SUM(to_block - from_block + 1),0) FROM coverage WHERE recording=1")?,
        pools_verified: one(c, "SELECT COUNT(*) FROM pools WHERE verified=1")?,
        pools_rejected: one(c, "SELECT COUNT(*) FROM pools WHERE verified=0")?,
        arb_total: one(c, "SELECT COUNT(*) FROM opportunities WHERE suspect=0 AND close_kind='arb'")?,
        arb_same_block: one(c, "SELECT COUNT(*) FROM opportunities WHERE suspect=0 AND close_kind='arb' AND same_block=1")?,
        ..Default::default()
    };
    s.addressable_usd = c.query_row(
        "SELECT COALESCE(SUM(gross_usd - ?1),0) FROM opportunities
         WHERE suspect=0 AND close_kind='arb' AND same_block=0 AND gross_usd > ?1",
        [gas],
        |r| r.get(0),
    )?;
    let mut st = c.prepare("SELECT gross_usd FROM opportunities WHERE suspect=0 AND close_kind!='stale' ORDER BY gross_usd")?;
    s.clean = st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
    Ok(s)
}

#[derive(Debug, PartialEq)]
pub struct Verdict {
    pub pass: bool,
    pub reasons: Vec<String>,
}

pub fn gate(s: &Stats, cfg: &Config) -> Verdict {
    let g = &cfg.gate;
    let median = quantile(&s.clean, 0.5);
    let per_day = s.per_day(s.addressable_usd);
    let share = s.same_block_share();
    let mut reasons = Vec::new();
    let mut pass = true;
    if s.hours < 24.0 {
        pass = false;
        reasons.push(format!("數據還不到 24 小時（目前 {:.1} 小時），先不下結論", s.hours));
    }
    if median < g.min_median_gross_usd {
        pass = false;
        reasons.push(format!("流量問題：毛利中位數 ${median:.2} < ${:.2} → 計畫建議轉 B", g.min_median_gross_usd));
    }
    if per_day < g.min_addressable_usd_per_day {
        pass = false;
        reasons.push(format!(
            "可爭取利潤每天 ${per_day:.2} < ${:.2}，不夠付成本",
            g.min_addressable_usd_per_day
        ));
    }
    if share > g.max_same_block_share {
        pass = false;
        reasons.push(format!(
            "Fast Feed 不對稱：{:.0}% 的機會在同一個區塊就被吃掉 > {:.0}% → 評估 Fast Feed 或轉 B",
            share * 100.0,
            g.max_same_block_share * 100.0
        ));
    }
    if pass {
        reasons.push("三個指標都通過 → 可以進入合約開發與小額上線".into());
    }
    Verdict { pass, reasons }
}

fn table(c: &Connection, out: &mut String, sql: &str, header: &[&str]) -> Result<()> {
    let mut st = c.prepare(sql)?;
    let n = st.column_count();
    writeln!(out, "| {} |", header.join(" | "))?;
    writeln!(out, "|{}", "---|".repeat(header.len()))?;
    let mut rows = st.query([])?;
    let mut any = false;
    while let Some(r) = rows.next()? {
        any = true;
        let cells: Vec<String> = (0..n)
            .map(|i| match r.get_ref(i).unwrap() {
                rusqlite::types::ValueRef::Null => "–".into(),
                rusqlite::types::ValueRef::Integer(v) => v.to_string(),
                rusqlite::types::ValueRef::Real(v) => format!("{v:.2}"),
                rusqlite::types::ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
                rusqlite::types::ValueRef::Blob(_) => "<blob>".into(),
            })
            .collect();
        writeln!(out, "| {} |", cells.join(" | "))?;
    }
    if !any {
        writeln!(out, "| {} |", vec!["–"; header.len()].join(" | "))?;
    }
    writeln!(out)?;
    Ok(())
}

pub fn render(c: &Connection, cfg: &Config) -> Result<String> {
    let s = collect(c, cfg)?;
    let v = gate(&s, cfg);
    let gas = cfg.detect.gas_cost_usd;
    let mut o = String::new();
    writeln!(o, "# FlashArb 側錄報表\n")?;
    writeln!(o, "## 結論：{}\n", if v.pass { "通過" } else { "未通過" })?;
    for r in &v.reasons {
        writeln!(o, "- {r}")?;
    }
    writeln!(o, "\n## 覆蓋範圍\n")?;
    writeln!(o, "- 記錄時數：{:.1} 小時，{} 個區塊", s.hours, s.blocks)?;
    writeln!(o, "- 已驗證池子：{}；被排除：{}", s.pools_verified, s.pools_rejected)?;
    writeln!(o, "\n## 三個判斷指標\n")?;
    writeln!(o, "| 指標 | 數值 | 門檻 |\n|---|---|---|")?;
    writeln!(o, "| 毛利中位數 | ${:.2} | ≥ ${:.2} |", quantile(&s.clean, 0.5), cfg.gate.min_median_gross_usd)?;
    writeln!(
        o,
        "| 每天可爭取淨利（被套利者吃掉、非同區塊，扣 gas ${gas:.2}） | ${:.2} | ≥ ${:.2} |",
        s.per_day(s.addressable_usd),
        cfg.gate.min_addressable_usd_per_day
    )?;
    writeln!(o, "| 同區塊就被吃掉的比例 | {:.0}% | ≤ {:.0}% |\n", s.same_block_share() * 100.0, cfg.gate.max_same_block_share * 100.0)?;
    writeln!(o, "## 毛利分布（排除可疑與過期）\n")?;
    writeln!(
        o,
        "- 機會數：{}（約每天 {:.0} 個）\n- 中位數 ${:.2}，p75 ${:.2}，p90 ${:.2}，最大 ${:.2}\n",
        s.clean.len(),
        s.per_day(s.clean.len() as f64),
        quantile(&s.clean, 0.5),
        quantile(&s.clean, 0.75),
        quantile(&s.clean, 0.9),
        s.clean.last().copied().unwrap_or(0.0)
    )?;
    writeln!(o, "## 機會怎麼結束的\n")?;
    table(
        c,
        &mut o,
        "SELECT CASE
            WHEN suspect=1 THEN '可疑（價差過大）'
            WHEN close_kind='stale' THEN '過期（10 分鐘沒人動）'
            WHEN close_kind='organic' THEN '一般交易抹平'
            WHEN same_block=1 THEN '套利者：同一區塊'
            WHEN delay_blocks=1 THEN '套利者：下一區塊'
            WHEN delay_blocks<=5 THEN '套利者：2–5 區塊'
            ELSE '套利者：> 5 區塊' END AS k,
          COUNT(*), SUM(gross_usd), AVG(gross_usd)
         FROM opportunities GROUP BY k ORDER BY COUNT(*) DESC",
        &["結束方式", "次數", "毛利合計 $", "平均 $"],
    )?;
    writeln!(o, "## 主流幣 vs 長尾\n")?;
    table(
        c,
        &mut o,
        &format!(
            "SELECT CASE WHEN major=1 THEN '主流' ELSE '長尾' END, COUNT(*), SUM(gross_usd),
               SUM(CASE WHEN close_kind='arb' AND same_block=0 AND gross_usd>{gas} THEN gross_usd-{gas} ELSE 0 END)
             FROM opportunities WHERE suspect=0 AND close_kind!='stale' GROUP BY major"
        ),
        &["類別", "次數", "毛利合計 $", "可爭取淨利 $"],
    )?;
    writeln!(o, "## 最常出現的 DEX 組合（前 10）\n")?;
    table(
        c,
        &mut o,
        "SELECT CASE WHEN venue_a<venue_b THEN venue_a||' × '||venue_b ELSE venue_b||' × '||venue_a END AS v,
           COUNT(*), SUM(gross_usd)
         FROM opportunities WHERE suspect=0 AND close_kind!='stale' GROUP BY v ORDER BY SUM(gross_usd) DESC LIMIT 10",
        &["組合", "次數", "毛利合計 $"],
    )?;
    writeln!(o, "## 對手：吃掉機會的合約（前 10）\n")?;
    writeln!(o, "集中度越高，代表越少數的專業玩家在壟斷。\n")?;
    table(
        c,
        &mut o,
        "SELECT COALESCE(closer_sender,'(v2 池，無 sender)'), COUNT(*), SUM(gross_usd),
           ROUND(100.0*COUNT(*)/(SELECT COUNT(*) FROM opportunities WHERE close_kind='arb' AND suspect=0),1)
         FROM opportunities WHERE close_kind='arb' AND suspect=0
         GROUP BY closer_sender ORDER BY COUNT(*) DESC LIMIT 10",
        &["合約", "次數", "毛利合計 $", "佔比 %"],
    )?;
    writeln!(o, "## 對手出價（抽樣）\n")?;
    writeln!(o, "priority fee 佔機會毛利的比例，就是對手的 α。我們的 α 要比它高才搶得到。\n")?;
    table(
        c,
        &mut o,
        "SELECT COUNT(*), AVG(t.priority_usd), AVG(t.total_fee_usd),
           AVG(CASE WHEN o.gross_usd>0 THEN t.priority_usd/o.gross_usd END)
         FROM closer_txs t JOIN opportunities o ON o.id=t.opp_id",
        &["樣本數", "平均 priority fee $", "平均總手續費 $", "平均 α"],
    )?;
    writeln!(o, "## 沒被認得的 factory（前 5，看到大的可以補進 config.toml）\n")?;
    table(
        c,
        &mut o,
        "SELECT factory, events FROM unknown_factories ORDER BY events DESC LIMIT 5",
        &["factory", "事件數"],
    )?;
    writeln!(
        o,
        "## 已知的近似\n\n- CL 池用目前 tick 內的虛擬儲備估算，跨 tick 的大單會高估。\n- Aero CL 只算了 swap 前的 hook 費率，swap 後的加收沒算進去。\n- Algebra（Camelot v3）的動態費率只在發現池子時讀一次。\n- 「可爭取」是理論上限：還沒扣我們自己的延遲與勝率。"
    )?;
    Ok(o)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detector::{CloseKind, Opportunity};
    use crate::ledger::Ledger;
    use alloy::primitives::{Address, B256};
    use std::path::Path;

    fn cfg() -> Config {
        Config::load(Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/config.toml"))).unwrap()
    }

    fn opp(gross: f64, kind: CloseKind, open: u64, close: u64) -> Opportunity {
        Opportunity {
            pool_a: Address::repeat_byte(1),
            pool_b: Address::repeat_byte(2),
            venue_a: "uniswap_v3".into(),
            venue_b: "aero_cl".into(),
            token0: Address::repeat_byte(3),
            token1: Address::repeat_byte(4),
            major: false,
            open_block: open,
            open_tx: B256::ZERO,
            gross_usd: gross,
            peak_usd: gross,
            gap_pct: 1.0,
            suspect: false,
            min_depth_usd: 10_000.0,
            close_block: close,
            close_tx: Some(B256::ZERO),
            close_kind: kind,
            closer_sender: Some(Address::repeat_byte(9)),
        }
    }

    #[test]
    fn quantiles() {
        let v = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(quantile(&v, 0.0), 1.0);
        assert_eq!(quantile(&v, 1.0), 4.0);
        assert!((quantile(&v, 0.5) - 2.5).abs() < 1e-12);
        assert_eq!(quantile(&[], 0.5), 0.0);
    }

    #[test]
    fn gate_and_render() {
        let cfg = cfg();
        let l = Ledger::in_memory().unwrap();
        l.add_coverage(1, 345_600, 0, 86_400 * 2, true, 0).unwrap(); // 48h
        for i in 0..100u64 {
            l.insert_opportunity(&opp(1.0, CloseKind::Arb, i, i + 1), 0).unwrap();
        }
        let s = collect(&l.conn, &cfg).unwrap();
        assert!((s.hours - 48.0).abs() < 1e-9);
        assert_eq!(s.arb_total, 100);
        // 100 * (1.00 - 0.03) over 2 days = 48.5/day
        assert!((s.per_day(s.addressable_usd) - 48.5).abs() < 1e-6);
        assert!(gate(&s, &cfg).pass);

        // Make them all same-block -> Fast Feed asymmetry fails the gate.
        l.conn.execute("UPDATE opportunities SET same_block=1, delay_blocks=0", []).unwrap();
        let s = collect(&l.conn, &cfg).unwrap();
        let v = gate(&s, &cfg);
        assert!(!v.pass);
        assert!(v.reasons.iter().any(|r| r.contains("Fast Feed")));

        let md = render(&l.conn, &cfg).unwrap();
        assert!(md.contains("未通過"));
        assert!(md.contains("uniswap_v3"));
    }
}
