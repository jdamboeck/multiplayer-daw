//! The project on disk: the event log, the receipts of transactions, snapshots of the document,
//! and server identity.
//!
//! Every write is a commit with `synchronous=FULL` in WAL mode: when a write returns, it is on
//! disk, power loss or not. Measured on a developer machine (ext4 on NVMe) a commit of one event
//! and its receipt takes about 0.25 ms at the median and 0.5 ms at p99, some 3400 a second
//! (`tests/commit_bench.rs`), which is far more than people editing together produce; so each
//! transaction is committed on its own, without grouping.

use anyhow::{Context, Result, bail};
use mdaw_model::{ActorId, Project, UserId};
use mdaw_proto::{Event, RejectReason, TxnId};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

/// SQLite integers are signed. Sequence numbers and transaction IDs count up from zero and never
/// get near the sign bit.
fn int(n: u64) -> i64 {
    n as i64
}

/// Bumped by every change to the stored format that is not free; see
/// `docs/decisions/0003-stored-format.md`.
const STORE_VERSION: u32 = 8;

fn encode<T: serde::Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value).context("encoding for the store")
}

fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8], what: &str) -> Result<T> {
    serde_json::from_slice(bytes).with_context(|| format!("decoding {what}"))
}

/// What the server answered a transaction with, kept for when it arrives again.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Receipt {
    /// Sequenced as the event with this number.
    Accepted(u64),
    Rejected(RejectReason),
}

pub struct Store {
    db: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        // Every commit is on disk before it returns: events are acknowledged and broadcast only
        // after theirs.
        db.pragma_update(None, "synchronous", "FULL")?;
        Self::init(db).with_context(|| format!("in {}", path.display()))
    }

    /// A store that is gone when dropped.
    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(db: Connection) -> Result<Self> {
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value BLOB NOT NULL);
             CREATE TABLE IF NOT EXISTS events (seq INTEGER PRIMARY KEY, data BLOB NOT NULL);
             CREATE TABLE IF NOT EXISTS receipts (
                 user INTEGER NOT NULL,
                 txn BLOB NOT NULL,
                 seq INTEGER,
                 reject BLOB,
                 PRIMARY KEY (user, txn));
             CREATE TABLE IF NOT EXISTS snapshots (seq INTEGER PRIMARY KEY, data BLOB NOT NULL);
             CREATE TABLE IF NOT EXISTS users (
                 id INTEGER PRIMARY KEY,
                 key BLOB NOT NULL UNIQUE,
                 name TEXT NOT NULL);",
        )?;
        let store = Self { db };
        match store.meta("format")? {
            None => store.set_meta("format", STORE_VERSION.to_string().as_bytes())?,
            Some(found) => {
                let found = String::from_utf8_lossy(&found).into_owned();
                match found.parse::<u32>() {
                    Err(_) => bail!(
                        "the project was written in format {found}, from before formats could be migrated: \
                         move the data directory away to start a new project."
                    ),
                    Ok(version) if version == STORE_VERSION => {}
                    Ok(version) if version > STORE_VERSION => bail!(
                        "the project was written by a newer server (format {found}, this server reads up to \
                         {STORE_VERSION})."
                    ),
                    Ok(_) => bail!("format {found} cannot be migrated"),
                }
            }
        }
        Ok(store)
    }

    pub fn meta(&self, key: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .db
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| row.get(0))
            .optional()?)
    }

    /// The `synchronous` setting of the connection every write goes through: 2 is `FULL`.
    pub fn synchronous(&self) -> Result<i64> {
        Ok(self
            .db
            .pragma_query_value(None, "synchronous", |row| row.get(0))?)
    }

    pub fn set_meta(&self, key: &str, value: &[u8]) -> Result<()> {
        self.db.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = ?2",
            params![key, value],
        )?;
        Ok(())
    }

    /// Sequence number of the last event stored, 0 for an empty log.
    pub fn head(&self) -> Result<u64> {
        let head: i64 = self
            .db
            .query_row("SELECT coalesce(max(seq), 0) FROM events", [], |row| row.get(0))?;
        Ok(head as u64)
    }

    /// What the server answered `user`'s transaction `txn` with, if it has seen it.
    pub fn receipt(&self, user: UserId, txn: TxnId) -> Result<Option<Receipt>> {
        let row: Option<(Option<i64>, Option<Vec<u8>>)> = self
            .db
            .prepare_cached("SELECT seq, reject FROM receipts WHERE user = ?1 AND txn = ?2")?
            .query_row(params![user.0, txn.0.as_slice()], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()?;
        Ok(match row {
            None => None,
            Some((Some(seq), _)) => Some(Receipt::Accepted(seq as u64)),
            Some((None, Some(reject))) => Some(Receipt::Rejected(decode(&reject, "a stored refusal")?)),
            Some((None, None)) => bail!("a receipt without an outcome"),
        })
    }

    /// Appends `event`, the transaction `event.txn` of `user`, to the log together with its
    /// receipt, in one SQLite transaction: both are on disk when this returns, or neither is.
    pub fn append(&self, user: UserId, event: &Event) -> Result<()> {
        let data = encode(event)?;
        // Rolled back when dropped uncommitted, on any error below.
        let tx = self.db.unchecked_transaction()?;
        tx.prepare_cached("INSERT INTO events (seq, data) VALUES (?1, ?2)")?
            .execute(params![int(event.seq), data])?;
        tx.prepare_cached("INSERT INTO receipts (user, txn, seq) VALUES (?1, ?2, ?3)")?
            .execute(params![user.0, event.txn.0.as_slice(), int(event.seq)])?;
        tx.commit()?;
        Ok(())
    }

    /// Records that `user`'s transaction `txn` was refused, so that it is refused again for the
    /// same reason should it come again.
    pub fn reject(&self, user: UserId, txn: TxnId, reason: &RejectReason) -> Result<()> {
        let reject = encode(reason)?;
        let tx = self.db.unchecked_transaction()?;
        tx.prepare_cached("INSERT INTO receipts (user, txn, reject) VALUES (?1, ?2, ?3)")?
            .execute(params![user.0, txn.0.as_slice(), reject])?;
        tx.commit()?;
        Ok(())
    }

    /// Up to `limit` events after `seq`, in order.
    pub fn events_after(&self, seq: u64, limit: u64) -> Result<Vec<Event>> {
        let mut query = self
            .db
            .prepare_cached("SELECT data FROM events WHERE seq > ?1 ORDER BY seq LIMIT ?2")?;
        let rows = query.query_map(params![int(seq), int(limit)], |row| row.get::<_, Vec<u8>>(0))?;
        let mut events = Vec::new();
        for data in rows {
            events.push(decode(&data?, "a stored event")?);
        }
        Ok(events)
    }

    pub fn put_snapshot(&self, seq: u64, project: &Project) -> Result<()> {
        let data = encode(project)?;
        let tx = self.db.unchecked_transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO snapshots (seq, data) VALUES (?1, ?2)",
            params![int(seq), data],
        )?;
        // The log is the history; of the snapshots only the newest is ever read.
        tx.execute("DELETE FROM snapshots WHERE seq < ?1", [int(seq)])?;
        tx.commit()?;
        Ok(())
    }

    pub fn latest_snapshot(&self) -> Result<Option<(u64, Project)>> {
        let row: Option<(i64, Vec<u8>)> = self
            .db
            .query_row(
                "SELECT seq, data FROM snapshots ORDER BY seq DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match row {
            None => Ok(None),
            Some((seq, data)) => Ok(Some((seq as u64, decode(&data, "the snapshot")?))),
        }
    }

    /// The user behind a client install's key, created on first sight. Keeps the latest name.
    pub fn user(&self, key: &[u8; 16], name: &str) -> Result<UserId> {
        self.db.execute(
            "INSERT INTO users (key, name) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET name = ?2",
            params![key.as_slice(), name],
        )?;
        let id: u32 = self
            .db
            .query_row("SELECT id FROM users WHERE key = ?1", [key.as_slice()], |row| {
                row.get(0)
            })?;
        Ok(UserId(id))
    }

    /// An actor ID never handed out before, across restarts and crashes: entity IDs are minted
    /// under it.
    pub fn next_actor(&self) -> Result<ActorId> {
        let next = self.last_actor()?.0.checked_add(1).context("out of actor IDs")?;
        self.set_meta("last_actor", &next.to_le_bytes())?;
        Ok(ActorId(next))
    }

    /// Records the start of a run of the server: the last actor handed out before it, and the head
    /// of the log it started with. Returns every run so far, this one included, oldest first.
    pub fn start_run(&self, head: u64) -> Result<Vec<Run>> {
        let mut runs: Vec<Run> = match self.meta("runs")? {
            Some(bytes) => decode(&bytes, "the stored runs")?,
            None => Vec::new(),
        };
        runs.push(Run {
            last_actor: self.last_actor()?,
            head,
        });
        self.set_meta("runs", &encode(&runs)?)?;
        Ok(runs)
    }

    /// The actor ID handed out last, `ActorId(0)` if none was.
    pub fn last_actor(&self) -> Result<ActorId> {
        Ok(ActorId(match self.meta("last_actor")? {
            Some(bytes) => u32::from_le_bytes(bytes.as_slice().try_into().context("stored actor counter")?),
            None => 0,
        }))
    }
}

/// One start of the server.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct Run {
    /// Actors up to this one were handed out before the run.
    pub last_actor: ActorId,
    /// The head of the log when the run began. Since events are written with
    /// `synchronous=FULL` before anyone hears of them, a client of an earlier run cannot have
    /// received events past it that the disk lost; but a disk that acknowledges writes it has not
    /// made, or a data directory put back from a backup, can still lose them, and this keeps such
    /// a client from taking the events written since under those seqs for its own.
    pub head: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_rows_are_json() {
        let store = Store::in_memory().unwrap();
        let event = Event {
            seq: 1,
            actor: ActorId(1),
            user: UserId(1),
            wall_ts: 0,
            txn: TxnId([1; 16]),
            ops: Vec::new(),
            superseded: Vec::new(),
        };
        store.append(UserId(1), &event).unwrap();
        let data: Vec<u8> = store
            .db
            .query_row("SELECT data FROM events WHERE seq = 1", [], |row| row.get(0))
            .unwrap();
        let value = serde_json::from_slice::<serde_json::Value>(&data).unwrap();
        assert!(value.get("seq").is_some(), "{value}");
    }
}
