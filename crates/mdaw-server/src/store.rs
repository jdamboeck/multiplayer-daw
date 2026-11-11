//! The project on disk: the event log, snapshots of the document, and server identity.

use anyhow::{Context, Result, bail};
use mdaw_model::{ActorId, Project, UserId};
use mdaw_proto::{Event, PROTO_VERSION};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

/// SQLite integers are signed. Sequence numbers and transaction IDs count up from zero and never
/// get near the sign bit.
fn int(n: u64) -> i64 {
    n as i64
}

/// Bumped when the tables change.
const STORE_VERSION: u32 = 1;

pub struct Store {
    db: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "NORMAL")?;
        Self::init(db).with_context(|| format!("in {}", path.display()))
    }

    /// A store that is gone when dropped.
    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(db: Connection) -> Result<Self> {
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value BLOB NOT NULL);
             CREATE TABLE IF NOT EXISTS events (
                 seq INTEGER PRIMARY KEY,
                 actor INTEGER NOT NULL,
                 txn INTEGER,
                 data BLOB NOT NULL);
             CREATE INDEX IF NOT EXISTS events_by_actor ON events (actor, txn);
             CREATE TABLE IF NOT EXISTS snapshots (seq INTEGER PRIMARY KEY, data BLOB NOT NULL);
             CREATE TABLE IF NOT EXISTS users (
                 id INTEGER PRIMARY KEY,
                 key BLOB NOT NULL UNIQUE,
                 name TEXT NOT NULL);",
        )?;
        let store = Self { db };
        let format = format!("{STORE_VERSION}/{PROTO_VERSION}").into_bytes();
        match store.meta("format")? {
            None => store.set_meta("format", &format)?,
            Some(found) if found == format => {}
            Some(found) => bail!(
                "the project was written in format {}, this server uses {}. Formats are not migrated yet: \
                 move the data directory away to start a new project.",
                String::from_utf8_lossy(&found),
                String::from_utf8_lossy(&format)
            ),
        }
        Ok(store)
    }

    pub fn meta(&self, key: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .db
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| row.get(0))
            .optional()?)
    }

    /// Like [`Store::set_meta`], but on disk before it returns, power loss or not. Events are
    /// written with less care for speed: the last few may be gone after a crash of the machine.
    pub fn set_meta_durably(&self, key: &str, value: &[u8]) -> Result<()> {
        self.db.pragma_update(None, "synchronous", "FULL")?;
        let result = self.set_meta(key, value);
        self.db.pragma_update(None, "synchronous", "NORMAL")?;
        result
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

    pub fn append(&self, event: &Event) -> Result<()> {
        let data = postcard::to_stdvec(event)?;
        self.db
            .prepare_cached("INSERT INTO events (seq, actor, txn, data) VALUES (?1, ?2, ?3, ?4)")?
            .execute(params![
                int(event.seq),
                event.actor.0,
                event.client_txn_id().map(int),
                data
            ])?;
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
            events.push(postcard::from_bytes(&data?).context("decoding a stored event")?);
        }
        Ok(events)
    }

    /// The highest transaction ID of `actor` that made it into the log.
    pub fn last_txn(&self, actor: ActorId) -> Result<Option<u64>> {
        let last: Option<i64> =
            self.db
                .query_row("SELECT max(txn) FROM events WHERE actor = ?1", [actor.0], |row| {
                    row.get(0)
                })?;
        Ok(last.map(|txn| txn as u64))
    }

    pub fn put_snapshot(&self, seq: u64, project: &Project) -> Result<()> {
        let data = postcard::to_stdvec(project)?;
        self.db.execute(
            "INSERT OR REPLACE INTO snapshots (seq, data) VALUES (?1, ?2)",
            params![int(seq), data],
        )?;
        // The log is the history; of the snapshots only the newest is ever read.
        self.db
            .execute("DELETE FROM snapshots WHERE seq < ?1", [int(seq)])?;
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
            Some((seq, data)) => Ok(Some((
                seq as u64,
                postcard::from_bytes(&data).context("decoding the snapshot")?,
            ))),
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
        self.set_meta_durably("last_actor", &next.to_le_bytes())?;
        Ok(ActorId(next))
    }

    /// Records the start of a run of the server: the last actor handed out before it, and the head
    /// of the log it started with. Returns every run so far, this one included, oldest first.
    pub fn start_run(&self, head: u64) -> Result<Vec<Run>> {
        let mut runs: Vec<Run> = match self.meta("runs")? {
            Some(bytes) => postcard::from_bytes(&bytes).context("stored runs")?,
            None => Vec::new(),
        };
        runs.push(Run {
            last_actor: self.last_actor()?,
            head,
        });
        self.set_meta_durably("runs", &postcard::to_stdvec(&runs)?)?;
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
    /// The head of the log when the run began. Events after it that clients of earlier runs
    /// received may have been lost in a crash, and others written under their seqs.
    pub head: u64,
}
