//! This client's edits the server has not confirmed yet, on disk: written before they are sent,
//! so that neither a lost connection nor a crash of the client loses them.
//!
//! One directory per project, `<profile>/projects/<server fingerprint>/` (a server serves one
//! project, and a new project means a new data directory and so a new certificate):
//!
//! - `pending`: the transactions, oldest first, each a record of a `u32` little-endian length,
//!   four check bytes (the start of the BLAKE3 hash of the payload) and the postcard [`Txn`].
//!   A transaction is written to the operating system before it is sent, so a crash of the
//!   client never loses it. A thread of the outbox forces it to the disk (`sync_data`) within
//!   about 50 ms; the UI thread never waits for the disk, and a power cut may take the last
//!   moments.
//! - `blobs/<hash>`: the bytes of blobs a pending transaction names, written in place before the
//!   transaction and forced to the disk by the same thread, so they can be uploaded again after
//!   a restart. A blob is checked by its hash when it is read; one cut short is removed.
//! - `format`: the protocol version (`PROTO_VERSION`) the transactions are encoded in. An
//!   outbox that holds transactions of another version (or of a client from before this file)
//!   is refused, not misread: their ops would decode as something else, or not at all.
//!
//! Confirmed transactions are dropped by emptying the file, with the blobs, once nothing is
//! pending any more. Until then a confirmed transaction stays in the file; sent again after a
//! restart, the server answers it from its receipt and applies nothing. That is also why emptying
//! needs no sync: a power cut can bring back only confirmed transactions. A record cut short by a
//! crash is the last one, which was not sent yet, and is dropped on the next start; a write that
//! failed halfway is taken back at once, so the next record starts where a record may start.

use anyhow::{Context, Result, bail};
use mdaw_model::{BlobHash, Op};
use mdaw_net::JoinInfo;
use mdaw_proto::{PROTO_VERSION, Txn, TxnId, blob_hash};
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// The sync thread rests this long after a round of syncs, so that during a long drag the file
/// is forced to the disk at most twenty times a second.
const SYNC_GAP: Duration = Duration::from_millis(50);

const PENDING: &str = "pending";
const BLOBS: &str = "blobs";
const FORMAT: &str = "format";

pub struct Outbox {
    dir: PathBuf,
    file: File,
    /// The transactions in the file, in order.
    ids: Vec<TxnId>,
    /// A write failed halfway and could not be taken back: appending more would be unreadable.
    broken: bool,
    /// To the sync thread; `None` only while opening and after dropping.
    disk: Option<Sender<Disk>>,
    thread: Option<JoinHandle<()>>,
    /// What the sync thread could not do, for [`Outbox::keep`] to report.
    failed: Arc<Mutex<Option<String>>>,
}

/// What the sync thread is asked to do, in order.
enum Disk {
    /// Force this file's data to the disk (a handle to `pending`, or a blob just written).
    Sync(File),
    /// Answer when everything asked before this is done.
    Flush(Sender<()>),
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
        check_format(dir, !bytes.is_empty())?;
        let (txns, whole) = parse(&bytes);
        let mut blobs = HashMap::new();
        for hash in txns.iter().flat_map(|t| t.ops.iter().flat_map(Op::blobs)) {
            let path = blob_dir.join(hash.to_string());
            if let Ok(bytes) = std::fs::read(&path) {
                if blob_hash(&bytes) == hash {
                    blobs.insert(hash, Arc::from(bytes));
                } else {
                    // Cut short while it was written: `put_blob` would keep it as it is.
                    let _ = std::fs::remove_file(&path);
                }
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
            broken: false,
            disk: None,
            thread: None,
            failed: Arc::default(),
        };
        if whole != bytes.len() {
            // A record cut short: appending after it would make everything after unreadable.
            outbox
                .rewrite(&txns)
                .with_context(|| format!("repairing {}", path.display()))?;
        }
        let (disk, jobs) = std::sync::mpsc::channel();
        let failed = Arc::clone(&outbox.failed);
        let thread = std::thread::Builder::new()
            .name("mdaw-outbox-sync".into())
            .spawn(move || sync_thread(&jobs, &failed))
            .context("starting the thread that saves edits")?;
        outbox.disk = Some(disk);
        outbox.thread = Some(thread);
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
        append_record(&mut self.file, &record, &mut self.broken)?;
        self.ids.push(txn.id);
        let file = self.file.try_clone().context("saving an edit")?;
        self.send(Disk::Sync(file)).context("saving an edit")
    }

    /// Keeps a blob that a transaction about to be appended names, on disk, until no pending
    /// transaction names it any more.
    pub fn put_blob(&self, hash: BlobHash, bytes: &[u8]) -> Result<()> {
        let path = self.dir.join(BLOBS).join(hash.to_string());
        // A file of another length was cut short while it was written: write it again.
        if std::fs::metadata(&path).is_ok_and(|m| m.is_file() && m.len() == bytes.len() as u64) {
            return Ok(());
        }
        // In place: `open` drops a blob whose bytes do not hash to its name.
        let mut file = File::create(&path).context("saving a blob")?;
        if let Err(err) = file.write_all(bytes) {
            drop(file);
            let _ = std::fs::remove_file(&path);
            return Err(err).context("saving a blob");
        }
        self.send(Disk::Sync(file)).context("saving a blob")
    }

    /// Called often (every frame): reports what the sync thread could not do, and empties the
    /// file once nothing is pending any more.
    pub fn keep(&mut self, pending: &[&Txn]) -> Result<()> {
        let failed = self.failed.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(text) = failed {
            bail!("{text}");
        }
        if pending.is_empty() && !self.ids.is_empty() {
            self.file.set_len(0).context("dropping confirmed edits")?;
            // Nothing torn is left to append after.
            self.broken = false;
            self.ids.clear();
            if let Ok(entries) = std::fs::read_dir(self.dir.join(BLOBS)) {
                for entry in entries.filter_map(|e| e.ok()) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        Ok(())
    }

    /// Waits until everything written so far is on the disk. For tests and for shutdown.
    pub fn flush(&self) {
        let (reply, done) = std::sync::mpsc::channel();
        if let Some(disk) = &self.disk
            && disk.send(Disk::Flush(reply)).is_ok()
        {
            let _ = done.recv();
        }
    }

    fn send(&self, job: Disk) -> Result<()> {
        let Some(disk) = &self.disk else {
            bail!("the thread that saves edits is not running");
        };
        disk.send(job)
            .map_err(|_| anyhow::anyhow!("the thread that saves edits has stopped"))
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

impl Drop for Outbox {
    fn drop(&mut self) {
        self.flush();
        self.disk = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Runs on its own thread: does the jobs in order, until the outbox is dropped.
fn sync_thread(jobs: &Receiver<Disk>, failed: &Mutex<Option<String>>) {
    while let Ok(first) = jobs.recv() {
        let batch: Vec<Disk> = std::iter::once(first).chain(jobs.try_iter()).collect();
        let mut flushed = false;
        for job in batch {
            match job {
                Disk::Sync(file) => {
                    if let Err(err) = file.sync_data() {
                        *failed.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(format!("saving edits: {err}"));
                    }
                }
                Disk::Flush(reply) => {
                    let _ = reply.send(());
                    flushed = true;
                }
            }
        }
        if !flushed {
            std::thread::sleep(SYNC_GAP);
        }
    }
}

/// A file a record is appended to, and taken back from when the write stops halfway.
trait Rollback {
    fn size(&self) -> io::Result<u64>;
    fn cut_to(&self, size: u64) -> io::Result<()>;
}

impl Rollback for File {
    fn size(&self) -> io::Result<u64> {
        Ok(self.metadata()?.len())
    }

    fn cut_to(&self, size: u64) -> io::Result<()> {
        self.set_len(size)
    }
}

/// Appends a record whole or not at all: `write_all` may have written part of it when it
/// fails, and reading stops at the first record that does not check out (`parse`).
fn append_record<F: Write + Rollback>(file: &mut F, record: &[u8], broken: &mut bool) -> Result<()> {
    if *broken {
        bail!("the file of unsent edits is damaged; edits are no longer saved on this computer");
    }
    let before = file.size().context("saving an edit")?;
    if let Err(err) = file.write_all(record) {
        // Take the part of a record back, so the next one starts where a record may start.
        if file.cut_to(before).is_err() {
            *broken = true;
        }
        return Err(err).context("saving an edit");
    }
    Ok(())
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

/// Refuses an outbox whose transactions (`pending` is `true` when there are any) are of another
/// protocol version; marks one without any as this version's.
fn check_format(dir: &Path, pending: bool) -> Result<()> {
    let path = dir.join(FORMAT);
    let format = PROTO_VERSION.to_string();
    let found = match std::fs::read_to_string(&path) {
        Ok(found) => Some(found.trim().to_string()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    if found.as_deref() == Some(format.as_str()) {
        return Ok(());
    }
    if pending {
        let found = match found {
            Some(found) => format!("protocol {found}"),
            None => "an earlier protocol".to_string(),
        };
        bail!(
            "the unsent edits in {} were saved in {found}, this client uses protocol {format}. They \
             are not converted: move the directory away to join without them.",
            dir.display()
        );
    }
    let mut file = File::create(&path).with_context(|| format!("writing {}", path.display()))?;
    file.write_all(format.as_bytes())
        .and_then(|()| file.sync_data())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
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
    use std::time::Instant;

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
    fn edits_of_another_protocol_are_refused_not_misread() {
        let dir = tempfile::tempdir().unwrap();
        let (mut outbox, _) = Outbox::open(dir.path()).unwrap();
        outbox.append(&edit(1, None)).unwrap();
        drop(outbox);
        let refused = |found: &str| {
            let err = Outbox::open(dir.path()).err().expect("refused").to_string();
            assert!(
                err.contains(&format!(
                    "saved in {found}, this client uses protocol {PROTO_VERSION}"
                )) && err.contains("move the directory away"),
                "{err}"
            );
        };

        // From a client of an earlier protocol.
        std::fs::write(dir.path().join(FORMAT), "10").unwrap();
        refused("protocol 10");
        // From a client from before the outbox kept its format.
        std::fs::remove_file(dir.path().join(FORMAT)).unwrap();
        refused("an earlier protocol");
        // Refusing changed nothing: the edit is still there for the client it belongs to.
        std::fs::write(dir.path().join(FORMAT), PROTO_VERSION.to_string()).unwrap();
        assert_eq!(Outbox::open(dir.path()).unwrap().1.txns.len(), 1);

        // An outbox without edits is taken over, whatever wrote it.
        let empty = tempfile::tempdir().unwrap();
        std::fs::write(empty.path().join(PENDING), b"").unwrap();
        std::fs::write(empty.path().join(FORMAT), "10").unwrap();
        Outbox::open(empty.path()).unwrap();
        let format = std::fs::read_to_string(empty.path().join(FORMAT)).unwrap();
        assert_eq!(format, PROTO_VERSION.to_string());
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

        let blob = dir.path().join(BLOBS).join(hash.to_string());

        // `a` is confirmed: nothing is dropped while something is still pending...
        outbox.keep(&[&b]).unwrap();
        assert_eq!(outbox.len(), 2);
        assert!(blob.exists());
        // ...and everything once nothing is.
        outbox.keep(&[]).unwrap();
        assert!(outbox.is_empty());
        assert!(!blob.exists());
        drop(outbox);
        let (_, saved) = Outbox::open(dir.path()).unwrap();
        assert!(saved.txns.is_empty());
    }

    #[test]
    fn a_torn_blob_is_not_restored() {
        let dir = tempfile::tempdir().unwrap();
        let (mut outbox, _) = Outbox::open(dir.path()).unwrap();
        let hash = blob_hash(b"settings");
        let a = edit(1, Some(hash));
        outbox.append(&a).unwrap();
        let blob = dir.path().join(BLOBS).join(hash.to_string());
        std::fs::write(&blob, b"sett").unwrap();
        drop(outbox);

        let (_, saved) = Outbox::open(dir.path()).unwrap();
        assert_eq!(saved.txns, [a]);
        assert!(saved.blobs.is_empty());
        assert!(!blob.exists(), "removed, so that it can be written again");
    }

    /// Checks that `flush` returns and that a reopened outbox reads the edit. It cannot check
    /// that the bytes reached the disk: reopening reads the page cache whether or not anything
    /// was synced.
    #[test]
    fn an_edit_is_read_back_after_flush() {
        let dir = tempfile::tempdir().unwrap();
        let (mut outbox, _) = Outbox::open(dir.path()).unwrap();
        let a = edit(1, None);
        outbox.append(&a).unwrap();
        outbox.flush();
        drop(outbox);
        let (_, saved) = Outbox::open(dir.path()).unwrap();
        assert_eq!(saved.txns, [a]);
    }

    /// Writes half of the first write, then fails; `cut_to` fails too if `stuck`.
    struct CutShort {
        file: File,
        written: bool,
        stuck: bool,
    }

    impl Write for CutShort {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.written {
                return self.file.write(buf);
            }
            self.written = true;
            self.file.write_all(&buf[..buf.len() / 2])?;
            Err(io::Error::other("the disk is full"))
        }

        fn flush(&mut self) -> io::Result<()> {
            self.file.flush()
        }
    }

    impl Rollback for CutShort {
        fn size(&self) -> io::Result<u64> {
            self.file.size()
        }

        fn cut_to(&self, size: u64) -> io::Result<()> {
            if self.stuck {
                return Err(io::Error::other("read-only"));
            }
            self.file.cut_to(size)
        }
    }

    #[test]
    fn a_write_cut_short_is_taken_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PENDING);
        let open = || OpenOptions::new().create(true).append(true).open(&path).unwrap();
        let (a, b) = (edit(1, None), edit(2, None));

        let mut broken = false;
        let mut torn = CutShort {
            file: open(),
            written: false,
            stuck: false,
        };
        assert!(append_record(&mut torn, &record(&a).unwrap(), &mut broken).is_err());
        assert!(!broken);
        append_record(&mut open(), &record(&b).unwrap(), &mut broken).unwrap();
        let (txns, whole) = parse(&std::fs::read(&path).unwrap());
        assert_eq!(txns, std::slice::from_ref(&b));
        assert_eq!(whole, std::fs::metadata(&path).unwrap().len() as usize);

        // When the part cannot be taken back, nothing more is appended.
        let mut stuck = CutShort {
            file: open(),
            written: false,
            stuck: true,
        };
        assert!(append_record(&mut stuck, &record(&a).unwrap(), &mut broken).is_err());
        assert!(broken);
        assert!(append_record(&mut open(), &record(&a).unwrap(), &mut broken).is_err());
    }

    #[test]
    fn a_short_blob_file_is_written_again() {
        let dir = tempfile::tempdir().unwrap();
        let (outbox, _) = Outbox::open(dir.path()).unwrap();
        let bytes = b"settings".to_vec();
        let hash = blob_hash(&bytes);
        let blob = dir.path().join(BLOBS).join(hash.to_string());
        std::fs::write(&blob, &bytes[..bytes.len() / 2]).unwrap();

        outbox.put_blob(hash, &bytes).unwrap();
        assert_eq!(std::fs::read(&blob).unwrap(), bytes);
    }

    #[test]
    fn appending_works_again_once_the_file_is_emptied() {
        let dir = tempfile::tempdir().unwrap();
        let (mut outbox, _) = Outbox::open(dir.path()).unwrap();
        let a = edit(1, None);
        outbox.append(&a).unwrap();
        let mut stuck = CutShort {
            file: outbox.file.try_clone().unwrap(),
            written: false,
            stuck: true,
        };
        assert!(append_record(&mut stuck, &record(&edit(2, None)).unwrap(), &mut outbox.broken).is_err());
        assert!(outbox.broken);
        assert!(outbox.append(&edit(3, None)).is_err());

        // `a` is confirmed and nothing is pending: the file is emptied, the torn part with it.
        outbox.keep(&[]).unwrap();
        let c = edit(4, None);
        outbox.append(&c).unwrap();
        drop(outbox);
        let (_, saved) = Outbox::open(dir.path()).unwrap();
        assert_eq!(saved.txns, [c]);
    }

    /// What saving an edit costs on this machine, with and without waiting for the disk. Run
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
        for flush in [false, true] {
            let mut times = Vec::new();
            for n in 0..500 {
                let txn = edit(n as u8, None);
                let started = Instant::now();
                outbox.append(&txn).unwrap();
                if flush {
                    outbox.flush();
                }
                times.push(started.elapsed());
            }
            times.sort();
            eprintln!(
                "append{}: p50 {:.3} ms, p99 {:.3} ms",
                if flush { " + flush" } else { "" },
                times[250].as_secs_f64() * 1e3,
                times[495].as_secs_f64() * 1e3
            );
        }
    }
}
