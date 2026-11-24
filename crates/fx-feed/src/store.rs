//! SQLite storage: candle cache, harness state, and an append-only journal.

use anyhow::Result;
use fx_core::{Candle, Timeframe};
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};

const GENESIS: &str = "genesis";

pub struct Store {
    conn: Connection,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ChainStatus {
    pub entries: usize,
    pub intact: bool,
    pub broken_at: Option<i64>,
    pub head: Option<String>,
    pub since: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct JournalRow {
    pub id: i64,
    pub ts: i64,
    pub kind: String,
    pub text: String,
}

fn chain_hash(prev: &str, ts: i64, kind: &str, text: &str) -> String {
    let digest = Sha256::digest(format!("{prev}|{ts}|{kind}|{text}").as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn tf_key(tf: Timeframe) -> &'static str {
    match tf {
        Timeframe::M15 => "M15",
        Timeframe::H1 => "H1",
        Timeframe::H4 => "H4",
        Timeframe::D1 => "D1",
    }
}

impl Store {
    pub fn open(path: &str) -> Result<Self> {
        Self::init(Connection::open(path)?)
    }

    pub fn memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS candles(pair TEXT, tf TEXT, ts INTEGER, o REAL, h REAL, l REAL, c REAL, PRIMARY KEY(pair, tf, ts));
             CREATE TABLE IF NOT EXISTS kv(k TEXT PRIMARY KEY, v TEXT);
             CREATE TABLE IF NOT EXISTS journal(id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER, kind TEXT, text TEXT);",
        )?;
        // Older databases have a journal without hashes: add the columns, leave old rows unchained.
        for col in ["prev_hash", "hash"] {
            let _ = conn.execute(&format!("ALTER TABLE journal ADD COLUMN {col} TEXT"), []);
        }
        Ok(Self { conn })
    }

    pub fn upsert_candles(
        &mut self,
        pair: &str,
        tf: Timeframe,
        candles: &[Candle],
    ) -> Result<usize> {
        let tx = self.conn.transaction()?;
        {
            let mut st =
                tx.prepare_cached("INSERT OR REPLACE INTO candles VALUES(?1,?2,?3,?4,?5,?6,?7)")?;
            for c in candles {
                st.execute(params![
                    pair,
                    tf_key(tf),
                    c.ts,
                    c.open,
                    c.high,
                    c.low,
                    c.close
                ])?;
            }
        }
        tx.commit()?;
        Ok(candles.len())
    }

    /// The latest `limit` candles in time order.
    pub fn load(&self, pair: &str, tf: Timeframe, limit: usize) -> Result<Vec<Candle>> {
        let mut st = self.conn.prepare_cached(
            "SELECT ts,o,h,l,c FROM candles WHERE pair=?1 AND tf=?2 ORDER BY ts DESC LIMIT ?3",
        )?;
        let mut rows: Vec<Candle> = st
            .query_map(params![pair, tf_key(tf), limit as i64], |r| {
                Ok(Candle {
                    ts: r.get(0)?,
                    open: r.get(1)?,
                    high: r.get(2)?,
                    low: r.get(3)?,
                    close: r.get(4)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        rows.reverse();
        Ok(rows)
    }

    pub fn count(&self, pair: &str, tf: Timeframe) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM candles WHERE pair=?1 AND tf=?2",
            params![pair, tf_key(tf)],
            |r| r.get(0),
        )?)
    }

    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO kv VALUES(?1,?2)",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn get(&self, key: &str) -> Result<Option<String>> {
        let mut st = self.conn.prepare_cached("SELECT v FROM kv WHERE k=?1")?;
        let mut rows = st.query(params![key])?;
        Ok(rows.next()?.map(|r| r.get(0)).transpose()?)
    }

    /// Appends a journal row chained to the one before it: each row's hash covers the previous
    /// hash and its own content, so changing or removing an earlier row breaks every later hash.
    pub fn log(&self, ts: i64, kind: &str, text: &str) -> Result<i64> {
        let prev: String = self
            .conn
            .query_row(
                "SELECT hash FROM journal WHERE hash IS NOT NULL ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap_or_else(|_| GENESIS.to_string());
        let hash = chain_hash(&prev, ts, kind, text);
        self.conn.execute(
            "INSERT INTO journal(ts,kind,text,prev_hash,hash) VALUES(?1,?2,?3,?4,?5)",
            params![ts, kind, text, prev, hash],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Re-computes the whole chain. Rows written before chaining existed are skipped, but an
    /// unchained row appearing after the chain started counts as tampering.
    pub fn verify_chain(&self) -> Result<ChainStatus> {
        let mut st = self
            .conn
            .prepare("SELECT id,ts,kind,text,prev_hash,hash FROM journal ORDER BY id")?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?;
        let mut out = ChainStatus {
            entries: 0,
            intact: true,
            broken_at: None,
            head: None,
            since: None,
        };
        // The first chained row anchors the chain, so pruning old rows from the front stays valid.
        let mut expected_prev: Option<String> = None;
        for row in rows {
            let (id, ts, kind, text, prev, hash) = row?;
            let (Some(prev), Some(hash)) = (prev, hash) else {
                if out.since.is_some() && out.intact {
                    out.intact = false;
                    out.broken_at = Some(id);
                }
                continue;
            };
            out.since.get_or_insert(ts);
            out.entries += 1;
            let anchor = expected_prev.get_or_insert_with(|| prev.clone());
            if out.intact && (prev != *anchor || chain_hash(&prev, ts, &kind, &text) != hash) {
                out.intact = false;
                out.broken_at = Some(id);
            }
            expected_prev = Some(hash.clone());
            out.head = Some(hash);
        }
        Ok(out)
    }

    /// Every chained row as one JSON object per line, so the file can be checked elsewhere.
    pub fn export_journal(&self) -> Result<String> {
        let mut st = self.conn.prepare(
            "SELECT id,ts,kind,text,prev_hash,hash FROM journal WHERE hash IS NOT NULL ORDER BY id",
        )?;
        let lines = st
            .query_map([], |r| {
                Ok(serde_json::json!({
                    "id": r.get::<_, i64>(0)?, "ts": r.get::<_, i64>(1)?, "kind": r.get::<_, String>(2)?,
                    "text": r.get::<_, String>(3)?, "prev": r.get::<_, String>(4)?, "hash": r.get::<_, String>(5)?,
                }))
            })?
            .map(|v| v.map(|j| j.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(lines.join("\n"))
    }

    /// Keeps the newest `keep` journal rows. The chain stays valid from the oldest row kept.
    pub fn prune_journal(&self, keep: usize) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM journal WHERE id <= (SELECT COALESCE(MAX(id),0) FROM journal) - ?1",
            params![keep as i64],
        )?)
    }

    /// Writes several key-value pairs in one transaction: all or none.
    pub fn set_many(&self, rows: &[(String, String)]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for (k, v) in rows {
            tx.execute("INSERT OR REPLACE INTO kv VALUES(?1,?2)", params![k, v])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Open time of the newest stored candle.
    pub fn latest_ts(&self, pair: &str, tf: Timeframe) -> Result<Option<i64>> {
        Ok(self.conn.query_row(
            "SELECT MAX(ts) FROM candles WHERE pair=?1 AND tf=?2",
            params![pair, tf_key(tf)],
            |r| r.get(0),
        )?)
    }

    pub fn last_id(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COALESCE(MAX(id),0) FROM journal", [], |r| r.get(0))?)
    }

    /// Journal rows after `since` of the given kinds, oldest first, for alerts.
    pub fn events_since(
        &self,
        since: i64,
        kinds: &[&str],
        limit: usize,
    ) -> Result<Vec<JournalRow>> {
        let mut st = self.conn.prepare_cached(
            "SELECT id,ts,kind,text FROM journal WHERE id>?1 ORDER BY id LIMIT ?2",
        )?;
        let rows = st
            .query_map(params![since, limit as i64 * 8], |r| {
                Ok(JournalRow {
                    id: r.get(0)?,
                    ts: r.get(1)?,
                    kind: r.get(2)?,
                    text: r.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .filter(|r| kinds.contains(&r.kind.as_str()))
            .take(limit)
            .collect())
    }

    /// The latest `limit` journal rows, newest first.
    pub fn recent_journal(&self, limit: usize) -> Result<Vec<(i64, String, String)>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT ts,kind,text FROM journal ORDER BY id DESC LIMIT ?1")?;
        let rows = st
            .query_map(params![limit as i64], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(ts: i64) -> Candle {
        Candle {
            ts,
            open: 1.0,
            high: 1.1,
            low: 0.9,
            close: 1.05,
        }
    }

    #[test]
    fn upsert_is_idempotent_and_load_returns_latest_in_order() {
        let mut s = Store::memory().unwrap();
        s.upsert_candles("EURUSD", Timeframe::H1, &[c(1), c(2), c(3)])
            .unwrap();
        s.upsert_candles("EURUSD", Timeframe::H1, &[c(3), c(4)])
            .unwrap();
        assert_eq!(s.count("EURUSD", Timeframe::H1).unwrap(), 4);
        let got = s.load("EURUSD", Timeframe::H1, 2).unwrap();
        assert_eq!(got.iter().map(|x| x.ts).collect::<Vec<_>>(), vec![3, 4]);
        assert_eq!(s.count("EURUSD", Timeframe::M15).unwrap(), 0);
    }

    fn seeded() -> Store {
        let s = Store::memory().unwrap();
        s.log(10, "paper", "setup ready").unwrap();
        s.log(20, "paper", "opened").unwrap();
        s.log(30, "paper", "closed").unwrap();
        s
    }

    #[test]
    fn an_untouched_journal_verifies_and_reports_its_head() {
        let s = seeded();
        let v = s.verify_chain().unwrap();
        assert!(v.intact && v.entries == 3 && v.since == Some(10) && v.head.is_some());
        assert_eq!(s.export_journal().unwrap().lines().count(), 3);
    }

    #[test]
    fn editing_a_row_is_detected_at_that_row() {
        let s = seeded();
        s.conn
            .execute(
                "UPDATE journal SET text='closed at a profit' WHERE id=3",
                [],
            )
            .unwrap();
        let v = s.verify_chain().unwrap();
        assert!(!v.intact);
        assert_eq!(v.broken_at, Some(3));
    }

    #[test]
    fn deleting_a_middle_row_is_detected() {
        let s = seeded();
        s.conn
            .execute("DELETE FROM journal WHERE id=2", [])
            .unwrap();
        let v = s.verify_chain().unwrap();
        assert!(!v.intact && v.broken_at == Some(3));
    }

    #[test]
    fn an_unchained_row_slipped_in_later_is_detected_but_old_rows_are_fine() {
        let s = Store::memory().unwrap();
        s.conn
            .execute(
                "INSERT INTO journal(ts,kind,text) VALUES(1,'old','before chaining')",
                [],
            )
            .unwrap();
        s.log(10, "paper", "first chained").unwrap();
        assert!(s.verify_chain().unwrap().intact);
        s.conn
            .execute(
                "INSERT INTO journal(ts,kind,text) VALUES(11,'paper','forged')",
                [],
            )
            .unwrap();
        assert!(!s.verify_chain().unwrap().intact);
    }

    #[test]
    fn pruning_the_front_keeps_the_rest_verifiable() {
        let s = seeded();
        s.log(40, "paper", "later").unwrap();
        assert_eq!(s.prune_journal(2).unwrap(), 2);
        let v = s.verify_chain().unwrap();
        assert!(v.intact && v.entries == 2);
    }

    #[test]
    fn set_many_writes_all_rows_and_latest_ts_tracks_candles() {
        let mut s = Store::memory().unwrap();
        s.set_many(&[("a".into(), "1".into()), ("b".into(), "2".into())])
            .unwrap();
        assert_eq!(s.get("b").unwrap().as_deref(), Some("2"));
        assert_eq!(s.latest_ts("EURUSD", Timeframe::H1).unwrap(), None);
        s.upsert_candles("EURUSD", Timeframe::H1, &[c(5), c(9)])
            .unwrap();
        assert_eq!(s.latest_ts("EURUSD", Timeframe::H1).unwrap(), Some(9));
    }

    #[test]
    fn events_since_filters_by_kind_and_id() {
        let s = seeded();
        s.log(40, "feed", "problem").unwrap();
        let e = s.events_since(1, &["feed"], 10).unwrap();
        assert_eq!(e.len(), 1);
        assert_eq!(s.events_since(0, &["paper"], 2).unwrap().len(), 2);
        assert_eq!(s.last_id().unwrap(), 4);
    }

    #[test]
    fn kv_and_journal_round_trip() {
        let s = Store::memory().unwrap();
        assert_eq!(s.get("a").unwrap(), None);
        s.set("a", "1").unwrap();
        s.set("a", "2").unwrap();
        assert_eq!(s.get("a").unwrap().as_deref(), Some("2"));
        s.log(10, "note", "first").unwrap();
        s.log(11, "note", "second").unwrap();
        assert_eq!(s.recent_journal(1).unwrap()[0].2, "second");
    }
}
