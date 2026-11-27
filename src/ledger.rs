//! SQLite ledger: every detected opportunity, the pools we have classified,
//! the blocks we have covered, and fee data for competitors' arbitrage txs.

use crate::detector::{Opportunity, PoolInfo};
use alloy::primitives::{Address, B256};
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

pub struct Ledger {
    pub conn: Connection,
}

#[derive(Debug, Clone)]
pub struct PoolRow {
    pub info: PoolInfo,
    pub factory: Address,
    pub verified: bool,
    pub note: String,
}

pub struct CloserTx {
    pub tx: B256,
    pub opp_id: i64,
    pub from: Address,
    pub gas_used: u64,
    pub effective_gas_price: u128,
    pub base_fee: u128,
    pub priority_usd: f64,
    pub total_fee_usd: f64,
}

/// Activity in one block. A "call" is a tx that emitted logs or reverted
/// (plain transfers do neither); reverted calls are mostly bots that lost a race.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BlockStats {
    pub block: u64,
    pub txs: u64,
    pub calls: u64,
    pub reverted: u64,
    pub gas: u64,
    pub reverted_gas: u64,
}

impl BlockStats {
    /// `txs` yields (succeeded, emitted_logs, gas_used) per transaction.
    pub fn tally(block: u64, txs: impl IntoIterator<Item = (bool, bool, u64)>) -> Self {
        let mut b = BlockStats { block, ..Default::default() };
        for (ok, logs, gas) in txs {
            b.txs += 1;
            b.gas += gas;
            if !ok {
                b.reverted += 1;
                b.reverted_gas += gas;
            }
            if logs || !ok {
                b.calls += 1;
            }
        }
        b
    }
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS pools (
  address TEXT PRIMARY KEY, factory TEXT NOT NULL, venue TEXT NOT NULL,
  token0 TEXT NOT NULL, token1 TEXT NOT NULL, fee_ppm INTEGER NOT NULL,
  is_cl INTEGER NOT NULL, verified INTEGER NOT NULL, note TEXT NOT NULL,
  tick_spacing INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS unknown_factories (factory TEXT PRIMARY KEY, events INTEGER NOT NULL);
-- How busy the blocks are where a rival arb landed: contract calls vs. ones that reverted.
CREATE TABLE IF NOT EXISTS block_stats (
  block INTEGER PRIMARY KEY, txs INTEGER NOT NULL, calls INTEGER NOT NULL,
  reverted INTEGER NOT NULL, gas INTEGER NOT NULL, reverted_gas INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS coverage (
  from_block INTEGER NOT NULL, to_block INTEGER NOT NULL,
  from_ts INTEGER NOT NULL, to_ts INTEGER NOT NULL,
  recording INTEGER NOT NULL, events INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS opportunities (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  pool_a TEXT NOT NULL, pool_b TEXT NOT NULL, venue_a TEXT NOT NULL, venue_b TEXT NOT NULL,
  token0 TEXT NOT NULL, token1 TEXT NOT NULL, major INTEGER NOT NULL,
  open_block INTEGER NOT NULL, open_tx TEXT NOT NULL,
  gross_usd REAL NOT NULL, peak_usd REAL NOT NULL, gap_pct REAL NOT NULL,
  suspect INTEGER NOT NULL, min_depth_usd REAL NOT NULL,
  close_block INTEGER NOT NULL, close_tx TEXT, close_kind TEXT NOT NULL,
  closer_sender TEXT, delay_blocks INTEGER NOT NULL, same_block INTEGER NOT NULL,
  recorded_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS opp_open_block ON opportunities(open_block);
CREATE TABLE IF NOT EXISTS closer_txs (
  tx TEXT PRIMARY KEY, opp_id INTEGER NOT NULL, from_addr TEXT NOT NULL,
  gas_used INTEGER NOT NULL, effective_gas_price TEXT NOT NULL, base_fee TEXT NOT NULL,
  priority_usd REAL NOT NULL, total_fee_usd REAL NOT NULL
);
"#;

/// Columns added after the first release. Each step is idempotent.
fn migrate(conn: &Connection) -> Result<()> {
    let has_open_ts: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('opportunities') WHERE name='open_ts'")?
        .exists([])?;
    if !has_open_ts {
        conn.execute_batch(
            "ALTER TABLE opportunities ADD COLUMN open_ts INTEGER NOT NULL DEFAULT 0;
             UPDATE opportunities SET open_ts = recorded_at WHERE open_ts = 0;",
        )?;
    }
    let has_ts: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('pools') WHERE name='tick_spacing'")?
        .exists([])?;
    if !has_ts {
        conn.execute_batch("ALTER TABLE pools ADD COLUMN tick_spacing INTEGER NOT NULL DEFAULT 0;")?;
    }
    conn.execute_batch("CREATE INDEX IF NOT EXISTS opp_open_ts ON opportunities(open_ts);")?;
    Ok(())
}

/// Open a second, read-only connection for the dashboard.
pub fn open_read_only(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    Ok(conn)
}

fn s<T: std::fmt::Display>(v: T) -> String {
    v.to_string()
}

impl Ledger {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Self { conn })
    }

    #[cfg(test)]
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Self { conn })
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0)).optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Drop everything derived from pool classification so the next run rebuilds it.
    pub fn reset(&self) -> Result<()> {
        self.conn.execute_batch(
            "DELETE FROM pools; DELETE FROM unknown_factories; DELETE FROM coverage;
             DELETE FROM opportunities; DELETE FROM closer_txs; DELETE FROM block_stats; DELETE FROM meta;",
        )?;
        Ok(())
    }

    pub fn last_block(&self) -> Result<Option<u64>> {
        let v: Option<String> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key='last_block'", [], |r| r.get(0))
            .optional()?;
        Ok(v.and_then(|v| v.parse().ok()))
    }

    pub fn set_last_block(&self, b: u64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key,value) VALUES('last_block',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![b.to_string()],
        )?;
        Ok(())
    }

    pub fn upsert_pool(&self, row: &PoolRow) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO pools(address,factory,venue,token0,token1,fee_ppm,is_cl,verified,note,tick_spacing)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                s(row.info.address),
                s(row.factory),
                row.info.venue,
                s(row.info.token0),
                s(row.info.token1),
                row.info.fee_ppm,
                row.info.is_cl as i64,
                row.verified as i64,
                row.note,
                row.info.tick_spacing
            ],
        )?;
        Ok(())
    }

    pub fn load_pools(&self) -> Result<Vec<PoolRow>> {
        let mut st = self.conn.prepare(
            "SELECT address,factory,venue,token0,token1,fee_ppm,is_cl,verified,note,tick_spacing FROM pools",
        )?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, u32>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, String>(8)?,
                r.get::<_, i32>(9)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (a, f, venue, t0, t1, fee, cl, ver, note, ts) = row?;
            out.push(PoolRow {
                info: PoolInfo {
                    address: a.parse()?,
                    venue,
                    token0: t0.parse()?,
                    token1: t1.parse()?,
                    fee_ppm: fee,
                    is_cl: cl != 0,
                    tick_spacing: ts,
                },
                factory: f.parse()?,
                verified: ver != 0,
                note,
            });
        }
        Ok(out)
    }

    pub fn bump_unknown_factory(&self, factory: Address, events: u64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO unknown_factories(factory,events) VALUES(?1,?2)
             ON CONFLICT(factory) DO UPDATE SET events = events + excluded.events",
            params![s(factory), events as i64],
        )?;
        Ok(())
    }

    pub fn add_coverage(&self, from: u64, to: u64, from_ts: u64, to_ts: u64, recording: bool, events: u64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO coverage(from_block,to_block,from_ts,to_ts,recording,events) VALUES(?1,?2,?3,?4,?5,?6)",
            params![from as i64, to as i64, from_ts as i64, to_ts as i64, recording as i64, events as i64],
        )?;
        Ok(())
    }

    pub fn insert_opportunity(&self, o: &Opportunity, now: i64, open_ts: i64) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO opportunities(pool_a,pool_b,venue_a,venue_b,token0,token1,major,open_block,open_tx,
               gross_usd,peak_usd,gap_pct,suspect,min_depth_usd,close_block,close_tx,close_kind,closer_sender,
               delay_blocks,same_block,recorded_at,open_ts)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22)",
            params![
                s(o.pool_a),
                s(o.pool_b),
                o.venue_a,
                o.venue_b,
                s(o.token0),
                s(o.token1),
                o.major as i64,
                o.open_block as i64,
                s(o.open_tx),
                o.gross_usd,
                o.peak_usd,
                o.gap_pct,
                o.suspect as i64,
                o.min_depth_usd,
                o.close_block as i64,
                o.close_tx.map(s),
                o.close_kind.as_str(),
                o.closer_sender.map(s),
                o.delay_blocks() as i64,
                o.same_block() as i64,
                now,
                open_ts
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn closer_tx_count(&self) -> Result<u64> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM closer_txs", [], |r| r.get::<_, i64>(0))? as u64)
    }

    pub fn has_block_stats(&self, block: u64) -> Result<bool> {
        Ok(self.conn.prepare("SELECT 1 FROM block_stats WHERE block=?1")?.exists([block as i64])?)
    }

    pub fn insert_block_stats(&self, b: &BlockStats) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO block_stats(block,txs,calls,reverted,gas,reverted_gas) VALUES(?1,?2,?3,?4,?5,?6)",
            params![b.block as i64, b.txs as i64, b.calls as i64, b.reverted as i64, b.gas as i64, b.reverted_gas as i64],
        )?;
        Ok(())
    }

    pub fn insert_closer_tx(&self, c: &CloserTx) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO closer_txs(tx,opp_id,from_addr,gas_used,effective_gas_price,base_fee,priority_usd,total_fee_usd)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                s(c.tx),
                c.opp_id,
                s(c.from),
                c.gas_used as i64,
                c.effective_gas_price.to_string(),
                c.base_fee.to_string(),
                c.priority_usd,
                c.total_fee_usd
            ],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_stats_count_reverted_calls() {
        let b = BlockStats::tally(7, [(true, true, 100), (false, false, 50), (true, false, 21_000), (false, false, 30)]);
        assert_eq!(b, BlockStats { block: 7, txs: 4, calls: 3, reverted: 2, gas: 21_180, reverted_gas: 80 });
    }
    use crate::detector::CloseKind;

    #[test]
    fn roundtrip_pool_and_opportunity() {
        let l = Ledger::in_memory().unwrap();
        assert_eq!(l.last_block().unwrap(), None);
        l.set_last_block(42).unwrap();
        l.set_last_block(43).unwrap();
        assert_eq!(l.last_block().unwrap(), Some(43));

        let info = PoolInfo {
            address: Address::repeat_byte(9),
            venue: "aero_cl".into(),
            token0: Address::repeat_byte(1),
            token1: Address::repeat_byte(2),
            fee_ppm: 500,
            is_cl: true, tick_spacing: 0
        };
        l.upsert_pool(&PoolRow { info: info.clone(), factory: Address::repeat_byte(7), verified: true, note: String::new() })
            .unwrap();
        let pools = l.load_pools().unwrap();
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].info.address, info.address);
        assert!(pools[0].verified);

        let o = Opportunity {
            pool_a: info.address,
            pool_b: Address::repeat_byte(8),
            venue_a: "a".into(),
            venue_b: "b".into(),
            token0: info.token0,
            token1: info.token1,
            major: false,
            open_block: 10,
            open_tx: B256::ZERO,
            gross_usd: 1.0,
            peak_usd: 1.2,
            gap_pct: 1.0,
            suspect: false,
            min_depth_usd: 5000.0,
            close_block: 11,
            close_tx: Some(B256::repeat_byte(3)),
            close_kind: CloseKind::Arb,
            closer_sender: None,
        };
        let id = l.insert_opportunity(&o, 0, 0).unwrap();
        assert!(id > 0);
        l.bump_unknown_factory(Address::repeat_byte(5), 3).unwrap();
        l.bump_unknown_factory(Address::repeat_byte(5), 4).unwrap();
        let n: i64 = l.conn.query_row("SELECT events FROM unknown_factories", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 7);
    }
}
