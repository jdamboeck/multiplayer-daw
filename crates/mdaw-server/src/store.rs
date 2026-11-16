//! The project on disk: the event log, the receipts of transactions, snapshots of the document,
//! and server identity.
//!
//! Every write is a commit with `synchronous=FULL` in WAL mode: when a write returns, it is on
//! disk, power loss or not. Measured on a developer machine (ext4 on NVMe) a commit of one event
//! and its receipt takes about 0.25 ms at the median and 0.5 ms at p99, some 3400 a second
//! (`tests/commit_bench.rs`), which is far more than people editing together produce; so each
//! transaction is committed on its own, without grouping.

use crate::migrate;
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
const STORE_VERSION: u32 = 9;

// The version and the migration steps cannot drift apart.
const _: () = assert!(STORE_VERSION == migrate::FIRST + migrate::STEPS.len() as u32);

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
        Self::back_up_before_migrating(&db, path).with_context(|| format!("in {}", path.display()))?;
        Self::init(db).with_context(|| format!("in {}", path.display()))
    }

    /// Copies a store of an older version to `<path>.v<version>` before it is migrated, unless
    /// that copy exists already. The copy is written as `<path>.v<version>.part` and renamed when
    /// whole, so a crash or a full disk during it leaves no partial backup under the final name.
    fn back_up_before_migrating(db: &Connection, path: &Path) -> Result<()> {
        let found = match db.query_row("SELECT value FROM meta WHERE key = 'format'", [], |row| {
            row.get::<_, Vec<u8>>(0)
        }) {
            Ok(found) => found,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(()),
            // A new file: `init` has not created the table yet.
            Err(error) if error.to_string().contains("no such table") => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let Ok(version) = String::from_utf8_lossy(&found).parse::<u32>() else {
            return Ok(());
        };
        if version >= STORE_VERSION {
            return Ok(());
        }
        let mut backup = path.as_os_str().to_owned();
        backup.push(format!(".v{version}"));
        let backup = std::path::PathBuf::from(backup);
        if backup.exists() {
            return Ok(());
        }
        let mut part = backup.as_os_str().to_owned();
        part.push(".part");
        let part = std::path::PathBuf::from(part);
        match std::fs::remove_file(&part) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(error).with_context(|| format!("removing a leftover {}", part.display()));
            }
            _ => {}
        }
        let part_str = part.to_str().context("a backup path that is not UTF-8")?;
        db.execute("VACUUM INTO ?1", [part_str])
            .with_context(|| format!("copying the store to {part_str} before migrating it"))?;
        std::fs::rename(&part, &backup)
            .with_context(|| format!("renaming {} to {}", part.display(), backup.display()))?;
        let dir = match path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir,
            _ => Path::new("."),
        };
        std::fs::File::open(dir)
            .and_then(|dir| dir.sync_all())
            .with_context(|| format!("syncing {}", dir.display()))?;
        Ok(())
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
                    Ok(version) if (migrate::FIRST..STORE_VERSION).contains(&version) => migrate::run(
                        &store.db,
                        version,
                        &migrate::STEPS[(version - migrate::FIRST) as usize..],
                    )?,
                    Ok(version) if version > STORE_VERSION => bail!(
                        "the project was written by a newer server (format {found}, this server reads up to \
                         {STORE_VERSION})."
                    ),
                    Ok(_) => bail!(
                        "the project was written in format {found}, from before formats could be migrated: \
                         move the data directory away to start a new project."
                    ),
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

    /// Writes a store at format 8 to `path`, with one event and one snapshot as format 8 holds
    /// them.
    fn write_store_8(path: &Path) {
        use crate::migrate::tests::{EVENT_8, PROJECT_8};
        let db = Connection::open(path).unwrap();
        db.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value BLOB NOT NULL);
             CREATE TABLE events (seq INTEGER PRIMARY KEY, data BLOB NOT NULL);
             CREATE TABLE snapshots (seq INTEGER PRIMARY KEY, data BLOB NOT NULL);",
        )
        .unwrap();
        db.execute(
            "INSERT INTO meta (key, value) VALUES ('format', ?1)",
            [b"8".as_slice()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO events (seq, data) VALUES (1, ?1)",
            [EVENT_8.as_bytes()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO snapshots (seq, data) VALUES (0, ?1)",
            [PROJECT_8.as_bytes()],
        )
        .unwrap();
    }

    /// Opens a store at format 8 at `path` and checks that it reads as format 9, with its event
    /// and its snapshot.
    fn open_and_check_migrated(path: &Path) {
        let store = Store::open(path).unwrap();
        assert_eq!(store.meta("format").unwrap().as_deref(), Some(b"9".as_slice()));
        let events = store.events_after(0, 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].seq, 1);
        assert_eq!(events[0].ops.len(), 2);
        let (seq, project) = store.latest_snapshot().unwrap().unwrap();
        assert_eq!(seq, 0);
        assert_eq!(project.tracks.len(), 1);
    }

    fn backup_path(path: &Path, suffix: &str) -> std::path::PathBuf {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        name.into()
    }

    #[test]
    fn an_old_store_is_backed_up_and_migrated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("project.sqlite");
        write_store_8(&path);
        open_and_check_migrated(&path);

        let backup = backup_path(&path, ".v8");
        assert!(backup.exists(), "no backup at {}", backup.display());
        let old = Connection::open(&backup).unwrap();
        let format: Vec<u8> = old
            .query_row("SELECT value FROM meta WHERE key = 'format'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(format, b"8");
        assert!(!backup_path(&path, ".v8.part").exists());
    }

    #[test]
    fn a_partial_backup_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("project.sqlite");
        write_store_8(&path);
        let part = backup_path(&path, ".v8.part");
        std::fs::write(&part, b"not a database, cut off halfway").unwrap();
        open_and_check_migrated(&path);

        assert!(!part.exists(), "{} is still there", part.display());
        let old = Connection::open(backup_path(&path, ".v8")).unwrap();
        let format: Vec<u8> = old
            .query_row("SELECT value FROM meta WHERE key = 'format'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(format, b"8");
    }

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

    #[test]
    fn parameter_values_come_back_exactly() {
        use mdaw_model::{DeviceId, EntityId, Op, TrackId};
        // xorshift64 with a fixed seed, turned into an f64 in 0..1 from its top 53 bits.
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f64 / (1u64 << 53) as f64
        };
        let mut values = vec![0.9856906946328695];
        values.extend((0..1000).map(|_| next()));

        let store = Store::in_memory().unwrap();
        for (i, &value) in values.iter().enumerate() {
            let seq = i as u64 + 1;
            let mut txn = [0; 16];
            txn[..8].copy_from_slice(&seq.to_le_bytes());
            let event = Event {
                seq,
                actor: ActorId(1),
                user: UserId(1),
                wall_ts: 0,
                txn: TxnId(txn),
                ops: vec![Op::DeviceSetParam {
                    track: TrackId(EntityId(1)),
                    id: DeviceId(EntityId(2)),
                    param: 27,
                    value: Some(value),
                }],
                superseded: Vec::new(),
            };
            store.append(UserId(1), &event).unwrap();
        }

        let events = store.events_after(0, values.len() as u64).unwrap();
        assert_eq!(events.len(), values.len());
        for (event, &value) in events.iter().zip(&values) {
            let [
                Op::DeviceSetParam {
                    value: Some(back), ..
                },
            ] = event.ops.as_slice()
            else {
                panic!("event {} came back as {:?}", event.seq, event.ops);
            };
            assert_eq!(
                back.to_bits(),
                value.to_bits(),
                "event {}: stored {value:?}, read back {back:?}",
                event.seq
            );
        }
    }
}
