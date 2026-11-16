//! Brings a store written by an earlier version of the server up to the current one; see
//! `docs/decisions/0003-stored-format.md`.

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};
use serde_json::Value;

/// Brings stored rows from one store version to the next. Each function changes one decoded
/// row in place.
pub(crate) struct Step {
    /// Every row of `events.data` (an `Event`).
    pub event: fn(&mut Value),
    /// Every row of `snapshots.data` (a `Project`).
    pub project: fn(&mut Value),
}

/// The first version that can be migrated from.
pub(crate) const FIRST: u32 = 8;

/// `STEPS[i]` brings a store from version `FIRST + i` to `FIRST + i + 1`.
pub(crate) const STEPS: &[Step] = &[];

/// Runs `steps` over a store of version `from`, checks that every row then decodes with the
/// current types, and writes the new version, all in one SQLite transaction: on any error
/// nothing has changed.
pub(crate) fn run(db: &Connection, from: u32, steps: &[Step]) -> Result<()> {
    // Rolled back when dropped uncommitted, on any error below.
    let tx = db.unchecked_transaction()?;
    for step in steps {
        rewrite(&tx, "events", step.event)?;
        rewrite(&tx, "snapshots", step.project)?;
    }
    for (seq, data) in rows(&tx, "events")? {
        if serde_json::from_slice::<mdaw_proto::Event>(&data).is_err() {
            bail!("after migrating, the row {seq} of events is not an event");
        }
    }
    for (seq, data) in rows(&tx, "snapshots")? {
        if serde_json::from_slice::<mdaw_model::Project>(&data).is_err() {
            bail!("after migrating, the row {seq} of snapshots is not a project");
        }
    }
    let to = from + steps.len() as u32;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES ('format', ?1) ON CONFLICT (key) DO UPDATE SET value = ?1",
        [to.to_string().as_bytes()],
    )?;
    tx.commit()?;
    Ok(())
}

/// Every `(seq, data)` of `table`, read in full before anything is written back.
fn rows(db: &Connection, table: &str) -> Result<Vec<(i64, Vec<u8>)>> {
    let mut query = db.prepare(&format!("SELECT seq, data FROM {table} ORDER BY seq"))?;
    let rows = query
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn rewrite(db: &Connection, table: &str, change: fn(&mut Value)) -> Result<()> {
    let mut update = db.prepare(&format!("UPDATE {table} SET data = ?2 WHERE seq = ?1"))?;
    for (seq, data) in rows(db, table)? {
        let mut value: Value =
            serde_json::from_slice(&data).with_context(|| format!("the row {seq} of {table} is not JSON"))?;
        change(&mut value);
        update.execute(params![seq, serde_json::to_vec(&value)?])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mdaw_model::{ActorId, Op, Project, UserId};
    use mdaw_proto::{Event, TxnId};
    use rusqlite::OptionalExtension;

    fn event(seq: u64) -> Event {
        Event {
            seq,
            actor: ActorId(1),
            user: UserId(1),
            wall_ts: 0,
            txn: TxnId([seq as u8; 16]),
            ops: vec![Op::MasterSetGain { gain: 0.5 }],
            superseded: Vec::new(),
        }
    }

    /// A store of version 8 with two events and one snapshot.
    fn store() -> Connection {
        let db = Connection::open_in_memory().unwrap();
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
        for seq in [1, 2] {
            db.execute(
                "INSERT INTO events (seq, data) VALUES (?1, ?2)",
                params![seq as i64, serde_json::to_vec(&event(seq)).unwrap()],
            )
            .unwrap();
        }
        db.execute(
            "INSERT INTO snapshots (seq, data) VALUES (2, ?1)",
            [serde_json::to_vec(&Project::default()).unwrap()],
        )
        .unwrap();
        db
    }

    fn format(db: &Connection) -> Option<String> {
        db.query_row("SELECT value FROM meta WHERE key = 'format'", [], |row| {
            row.get::<_, Vec<u8>>(0)
        })
        .optional()
        .unwrap()
        .map(|bytes| String::from_utf8(bytes).unwrap())
    }

    fn all(db: &Connection, table: &str) -> Vec<(i64, Vec<u8>)> {
        rows(db, table).unwrap()
    }

    #[test]
    fn a_step_rewrites_every_row() {
        let db = store();
        let step = Step {
            event: |value| value["wall_ts"] = 7.into(),
            project: |value| value["name"] = "migrated".into(),
        };
        run(&db, 8, &[step]).unwrap();
        let events = all(&db, "events");
        assert_eq!(events.len(), 2);
        for (_, data) in events {
            assert_eq!(serde_json::from_slice::<Event>(&data).unwrap().wall_ts, 7);
        }
        let snapshots = all(&db, "snapshots");
        assert_eq!(snapshots.len(), 1);
        assert_eq!(
            serde_json::from_slice::<Project>(&snapshots[0].1).unwrap().name,
            "migrated"
        );
        assert_eq!(format(&db).as_deref(), Some("9"));
    }

    #[test]
    fn a_step_that_breaks_a_row_changes_nothing() {
        let db = store();
        let events = all(&db, "events");
        let snapshots = all(&db, "snapshots");
        let step = Step {
            event: |value| {
                value.as_object_mut().unwrap().remove("seq");
            },
            project: |_| {},
        };
        let error = run(&db, 8, &[step]).unwrap_err();
        assert!(error.to_string().contains("events"), "{error}");
        assert_eq!(all(&db, "events"), events);
        assert_eq!(all(&db, "snapshots"), snapshots);
        assert_eq!(format(&db).as_deref(), Some("8"));
    }

    #[test]
    fn no_steps_only_checks() {
        let db = store();
        run(&db, 8, &[]).unwrap();
        assert_eq!(format(&db).as_deref(), Some("8"));
    }
}
