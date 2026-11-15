//! What a durable commit costs on this machine: appends small transactions (an event row and a
//! receipt row, in one SQLite transaction, as the hub writes them) with `synchronous=NORMAL` and
//! with `synchronous=FULL`, and prints commits per second and the latency per commit.
//!
//! Run by hand:
//! `cargo test --release -p mdaw-server --test commit_bench -- --ignored --nocapture`
//! It writes under `target/phase5-scratch/`, or under `MDAW_BENCH_DIR` if set.

use rusqlite::{Connection, params};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const COMMITS: usize = 1000;

fn scratch() -> PathBuf {
    let dir = match std::env::var_os("MDAW_BENCH_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/phase5-scratch/bench"),
    };
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(synchronous: &str) -> Vec<Duration> {
    let path = scratch().join(format!("bench-{synchronous}.db"));
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    let mut db = Connection::open(&path).unwrap();
    db.pragma_update(None, "journal_mode", "WAL").unwrap();
    db.pragma_update(None, "synchronous", synchronous).unwrap();
    db.execute_batch(
        "CREATE TABLE events (seq INTEGER PRIMARY KEY, data BLOB NOT NULL);
         CREATE TABLE receipts (user INTEGER NOT NULL, txn BLOB NOT NULL, seq INTEGER, reject BLOB,
                                PRIMARY KEY (user, txn));",
    )
    .unwrap();
    // About the size of a note edit as postcard.
    let data = vec![7u8; 120];
    let mut times = Vec::with_capacity(COMMITS);
    for seq in 1..=COMMITS as i64 {
        let mut txn_id = [0u8; 16];
        txn_id[..8].copy_from_slice(&seq.to_le_bytes());
        let started = Instant::now();
        let tx = db.transaction().unwrap();
        tx.execute(
            "INSERT INTO events (seq, data) VALUES (?1, ?2)",
            params![seq, data],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO receipts (user, txn, seq) VALUES (?1, ?2, ?3)",
            params![1, txn_id.as_slice(), seq],
        )
        .unwrap();
        tx.commit().unwrap();
        times.push(started.elapsed());
    }
    times
}

fn report(name: &str, mut times: Vec<Duration>) {
    let total: Duration = times.iter().sum();
    times.sort();
    let at = |q: f64| times[((times.len() as f64 * q) as usize).min(times.len() - 1)];
    eprintln!(
        "{name:>6}: {:>8.0} commits/s, p50 {:>8.3} ms, p99 {:>8.3} ms, max {:>8.3} ms",
        COMMITS as f64 / total.as_secs_f64(),
        at(0.50).as_secs_f64() * 1e3,
        at(0.99).as_secs_f64() * 1e3,
        times.last().unwrap().as_secs_f64() * 1e3,
    );
}

#[test]
#[ignore = "a measurement, run by hand"]
fn commit_latency_normal_vs_full() {
    eprintln!(
        "{COMMITS} commits of one event and one receipt each, in {}",
        scratch().display()
    );
    for synchronous in ["NORMAL", "FULL", "NORMAL", "FULL"] {
        report(synchronous, run(synchronous));
    }
}
