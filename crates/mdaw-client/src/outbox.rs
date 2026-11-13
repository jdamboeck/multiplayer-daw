//! This client's edits the server has not confirmed yet, on disk: written before they are sent,
//! so that neither a lost connection nor a crash of the client loses them.
//!
//! One directory per project, `<profile>/projects/<server fingerprint>/` (a server serves one
//! project, and a new project means a new data directory and so a new certificate):
//!
//! - `pending`: the transactions, oldest first, each a record of a `u32` little-endian length,
//!   four check bytes (the start of the BLAKE3 hash of the payload) and the postcard [`Txn`].
//!   A transaction is appended and flushed to the operating system before it is sent. The file
//!   is forced to the disk (`sync_data`) at most [`SYNC_EVERY`] later, not on every edit: an
//!   edit is never lost to a crash of the client, while a power cut may take the last moments.
//! - `blobs/<hash>`: the bytes of blobs a pending transaction names, written (and forced to the
//!   disk) before the transaction, so they can be uploaded again after a restart.
//!
//! Confirmed transactions are dropped by writing the file anew (to the side, forced to disk,
//! then renamed over the old one), at most every [`COMPACT_EVERY`], or at once when nothing is
//! pending any more. Until then a confirmed transaction stays in the file; sent again after a
//! restart, the server answers it from its receipt and applies nothing. A crash at any point
//! leaves either the old file or the new one, never a transaction lost; a record cut short by a
//! crash is the last one, which was not sent yet, and is dropped on the next start.

use anyhow::{Context, Result};
use mdaw_model::{BlobHash, Op};
use mdaw_net::JoinInfo;
use mdaw_proto::{Txn, TxnId, blob_hash};
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Appended transactions reach the disk at most this long after they were written.
pub const SYNC_EVERY: Duration = Duration::from_millis(250);
/// Confirmed transactions are dropped from the file at most this often.
pub const COMPACT_EVERY: Duration = Duration::from_secs(1);

const PENDING: &str = "pending";
const BLOBS: &str = "blobs";

pub struct Outbox {
    dir: PathBuf,
    file: File,
    /// The transactions in the file, in order.
    ids: Vec<TxnId>,
    /// Written since the last `sync_data`.
    unsynced: bool,
    synced_at: Instant,
    compacted_at: Instant,
}

/// What an earlier run left in the outbox.
#[derive(Default)]
pub struct Saved {
    /// Oldest first.
    pub txns: Vec<Txn>,
    /// The bytes of blobs those transactions name, as far as they were kept.
    pub blobs: HashMap<BlobHash, Arc<[u8]>>,
}

impl Outbox {
    /// The outbox of the project a join string leads to, under the profile directory.
    pub fn dir_for(profile: &Path, join: &JoinInfo) -> PathBuf {
        let fingerprint: String = join.fingerprint.iter().map(|b| format!("{b:02x}")).collect();
        profile.join("projects").join(fingerprint)
    }

    /// Opens the outbox in `dir`, creating it if missing, and returns what it holds.
    pub fn open(dir: &Path) -> Result<(Self, Saved)> {
        let blob_dir = dir.join(BLOBS);
        std::fs::create_dir_all(&blob_dir).with_context(|| format!("creating {}", blob_dir.display()))?;
        let path = dir.join(PENDING);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        let (txns, whole) = parse(&bytes);
        let mut blobs = HashMap::new();
        for hash in txns.iter().flat_map(|t| t.ops.iter().flat_map(Op::blobs)) {
            if let Ok(bytes) = std::fs::read(blob_dir.join(hash.to_string()))
                && blob_hash(&bytes) == hash
            {
                blobs.insert(hash, Arc::from(bytes));
            }
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        sync_dir(dir)?;
        let mut outbox = Self {
            dir: dir.to_path_buf(),
            file,
            ids: txns.iter().map(|t| t.id).collect(),
            unsynced: false,
            synced_at: Instant::now(),
            compacted_at: Instant::now(),
        };
        if whole != bytes.len() {
            // A record cut short: appending after it would make everything after unreadable.
            outbox
                .rewrite(&txns)
                .with_context(|| format!("repairing {}", path.display()))?;
        }
        Ok((outbox, Saved { txns, blobs }))
    }

    /// Transactions in the file, confirmed or not.
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Keeps a transaction until it is confirmed. When this returns, a crash of the client no
    /// longer loses it; call before sending it.
    pub fn append(&mut self, txn: &Txn) -> Result<()> {
        let record = record(txn)?;
        self.file.write_all(&record).context("saving an edit")?;
        self.ids.push(txn.id);
        self.unsynced = true;
        Ok(())
    }

    /// Keeps a blob that a transaction about to be appended names, on disk, until no pending
    /// transaction names it any more.
    pub fn put_blob(&self, hash: BlobHash, bytes: &[u8]) -> Result<()> {
        let dir = self.dir.join(BLOBS);
        let path = dir.join(hash.to_string());
        if path.is_file() {
            return Ok(());
        }
        let part = dir.join(format!("{hash}.part"));
        let mut file = File::create(&part).context("saving a blob")?;
        file.write_all(bytes).context("saving a blob")?;
        file.sync_data().context("saving a blob")?;
        std::fs::rename(&part, &path).context("saving a blob")?;
        sync_dir(&dir)
    }

    /// Called often (every frame): drops what is no longer pending from the file now and then,
    /// and forces what was appended to the disk once [`SYNC_EVERY`] has passed.
    pub fn keep(&mut self, pending: &[&Txn]) -> Result<()> {
        if self.ids.len() > pending.len() || self.has_stale(pending) {
            let due = pending.is_empty() || self.compacted_at.elapsed() >= COMPACT_EVERY;
            if due {
                let txns: Vec<Txn> = pending.iter().map(|&t| t.clone()).collect();
                return self.rewrite(&txns);
            }
        }
        if self.unsynced && self.synced_at.elapsed() >= SYNC_EVERY {
            self.sync()?;
        }
        Ok(())
    }

    /// Whether something appended waits to be forced to the disk.
    pub fn unsynced(&self) -> bool {
        self.unsynced
    }

    /// Forces everything appended to the disk now.
    pub fn sync(&mut self) -> Result<()> {
        self.file.sync_data().context("saving edits")?;
        self.unsynced = false;
        self.synced_at = Instant::now();
        Ok(())
    }

    fn has_stale(&self, pending: &[&Txn]) -> bool {
        let pending: HashSet<TxnId> = pending.iter().map(|t| t.id).collect();
        self.ids.iter().any(|id| !pending.contains(id))
    }

    /// Replaces the file with one holding `txns`, and drops the blobs none of them names.
    fn rewrite(&mut self, txns: &[Txn]) -> Result<()> {
        let path = self.dir.join(PENDING);
        let part = self.dir.join(format!("{PENDING}.part"));
        let mut bytes = Vec::new();
        for txn in txns {
            bytes.extend(record(txn)?);
        }
        let mut file = File::create(&part).context("saving edits")?;
        file.write_all(&bytes).context("saving edits")?;
        file.sync_data().context("saving edits")?;
        std::fs::rename(&part, &path).context("saving edits")?;
        sync_dir(&self.dir)?;
        self.file = OpenOptions::new()
            .append(true)
            .open(&path)
            .context("saving edits")?;
        self.ids = txns.iter().map(|t| t.id).collect();
        self.unsynced = false;
        self.synced_at = Instant::now();
        self.compacted_at = Instant::now();

        let named: HashSet<String> = txns
            .iter()
            .flat_map(|t| t.ops.iter().flat_map(Op::blobs))
            .map(|h| h.to_string())
            .collect();
        if let Ok(entries) = std::fs::read_dir(self.dir.join(BLOBS)) {
            for entry in entries.filter_map(|e| e.ok()) {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !named.contains(&name) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        Ok(())
    }
}

fn record(txn: &Txn) -> Result<Vec<u8>> {
    let payload = postcard::to_stdvec(txn).context("encoding an edit")?;
    let len = u32::try_from(payload.len()).context("an edit too large to save")?;
    let mut record = Vec::with_capacity(8 + payload.len());
    record.extend_from_slice(&len.to_le_bytes());
    record.extend_from_slice(&blob_hash(&payload).0[..4]);
    record.extend_from_slice(&payload);
    Ok(record)
}

/// The transactions of a `pending` file, and how many of its bytes are whole records. Reading
/// stops at the first record that is cut short or does not check out.
fn parse(bytes: &[u8]) -> (Vec<Txn>, usize) {
    let mut txns = Vec::new();
    let mut at = 0;
    while let Some(header) = bytes.get(at..at + 8) {
        let len = u32::from_le_bytes(header[..4].try_into().expect("four bytes")) as usize;
        let Some(payload) = bytes.get(at + 8..at + 8 + len) else {
            break;
        };
        if blob_hash(payload).0[..4] != header[4..8] {
            break;
        }
        let Ok(txn) = postcard::from_bytes::<Txn>(payload) else {
            break;
        };
        txns.push(txn);
        at += 8 + len;
    }
    (txns, at)
}

/// Makes a file created or renamed in `dir` survive a power cut.
fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(dir)
        .and_then(|dir| dir.sync_all())
        .with_context(|| format!("saving {}", dir.display()))?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mdaw_model::{Device, DeviceId, DeviceKind, EntityId, PluginFormat, TrackId};

    fn edit(n: u8, state: Option<BlobHash>) -> Txn {
        let device = Device::new(
            DeviceId(EntityId(u64::from(n))),
            DeviceKind::Plugin {
                format: PluginFormat::Clap,
                id: "x".into(),
                name: "Synth".into(),
                state,
            },
        );
        Txn {
            id: TxnId::random(),
            base_seq: u64::from(n),
            ops: vec![mdaw_model::Op::DeviceAdd {
                track: TrackId(EntityId(1)),
                device,
                after: None,
            }],
        }
    }

    #[test]
    fn what_was_appended_comes_back_after_a_crash() {
        let dir = tempfile::tempdir().unwrap();
        let (mut outbox, saved) = Outbox::open(dir.path()).unwrap();
        assert!(saved.txns.is_empty());
        let bytes = b"settings".to_vec();
        let hash = blob_hash(&bytes);
        outbox.put_blob(hash, &bytes).unwrap();
        let (a, b) = (edit(1, Some(hash)), edit(2, None));
        outbox.append(&a).unwrap();
        outbox.append(&b).unwrap();
        // No sync, no drop handler: what a crash leaves.
        std::mem::forget(outbox);

        let (_, saved) = Outbox::open(dir.path()).unwrap();
        assert_eq!(saved.txns, [a, b]);
        assert_eq!(saved.blobs.get(&hash).map(|b| b.to_vec()), Some(bytes));
    }

    #[test]
    fn a_record_cut_short_is_dropped_and_the_file_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let (mut outbox, _) = Outbox::open(dir.path()).unwrap();
        let a = edit(1, None);
        outbox.append(&a).unwrap();
        outbox.append(&edit(2, None)).unwrap();
        drop(outbox);
        let path = dir.path().join(PENDING);
        let len = std::fs::metadata(&path).unwrap().len();
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(len - 3).unwrap();

        let (mut outbox, saved) = Outbox::open(dir.path()).unwrap();
        assert_eq!(saved.txns, std::slice::from_ref(&a));
        let c = edit(3, None);
        outbox.append(&c).unwrap();
        drop(outbox);
        let (_, saved) = Outbox::open(dir.path()).unwrap();
        assert_eq!(saved.txns, [a, c], "appended after the repair, readable");
    }

    #[test]
    fn confirmed_transactions_and_their_blobs_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let (mut outbox, _) = Outbox::open(dir.path()).unwrap();
        let bytes = b"settings".to_vec();
        let hash = blob_hash(&bytes);
        outbox.put_blob(hash, &bytes).unwrap();
        let (a, b) = (edit(1, Some(hash)), edit(2, None));
        outbox.append(&a).unwrap();
        outbox.append(&b).unwrap();

        // `a` is confirmed. Not compacted at once while something is still pending...
        outbox.keep(&[&b]).unwrap();
        assert_eq!(outbox.len(), 2);
        // ...but once a second has passed.
        outbox.compacted_at -= COMPACT_EVERY;
        outbox.keep(&[&b]).unwrap();
        assert_eq!(outbox.len(), 1);
        assert!(!dir.path().join(BLOBS).join(hash.to_string()).exists());
        // And at once when nothing is pending.
        outbox.keep(&[]).unwrap();
        assert!(outbox.is_empty());
        drop(outbox);
        let (_, saved) = Outbox::open(dir.path()).unwrap();
        assert!(saved.txns.is_empty());
    }

    /// What saving an edit costs on this machine, with and without forcing it to the disk. Run
    /// by hand: `cargo test --release -p mdaw-client --lib outbox -- --ignored --nocapture`,
    /// with `MDAW_BENCH_DIR` on the disk to measure (the default temporary directory may be
    /// memory).
    #[test]
    #[ignore = "a measurement, run by hand"]
    fn append_cost() {
        let base = std::env::var_os("MDAW_BENCH_DIR").map(PathBuf::from);
        let dir = match &base {
            Some(base) => tempfile::tempdir_in(base).unwrap(),
            None => tempfile::tempdir().unwrap(),
        };
        let (mut outbox, _) = Outbox::open(dir.path()).unwrap();
        for sync in [false, true] {
            let mut times = Vec::new();
            for n in 0..500 {
                let txn = edit(n as u8, None);
                let started = Instant::now();
                outbox.append(&txn).unwrap();
                if sync {
                    outbox.sync().unwrap();
                }
                times.push(started.elapsed());
            }
            times.sort();
            eprintln!(
                "append{}: p50 {:.3} ms, p99 {:.3} ms",
                if sync { " + sync_data" } else { "" },
                times[250].as_secs_f64() * 1e3,
                times[495].as_secs_f64() * 1e3
            );
        }
    }
}
