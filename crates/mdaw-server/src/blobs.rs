//! Audio and plugin state, stored by the BLAKE3 hash of their bytes.

use anyhow::{Context, Result};
use mdaw_model::BlobHash;
use mdaw_proto::blob_hash;
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// The project's blobs. Cheap to clone; all clones share the same store. Blobs are written
/// once and never change, so readers need no coordination with writers.
#[derive(Clone)]
pub struct Blobs(Inner);

#[derive(Clone)]
enum Inner {
    Disk(PathBuf),
    Memory(Arc<Mutex<HashMap<BlobHash, Arc<[u8]>>>>),
}

impl Blobs {
    /// Blobs in files under `dir`, which is created if missing.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        // Halves of uploads that a crash interrupted.
        for entry in std::fs::read_dir(&dir)?.filter_map(|e| e.ok()) {
            if entry.path().extension().is_some_and(|x| x == "part") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        Ok(Self(Inner::Disk(dir)))
    }

    /// Blobs that live as long as the process, for tests.
    pub fn in_memory() -> Self {
        Self(Inner::Memory(Arc::default()))
    }

    pub fn has(&self, hash: &BlobHash) -> bool {
        match &self.0 {
            Inner::Disk(dir) => dir.join(hash.to_string()).is_file(),
            Inner::Memory(map) => map.lock().expect("blob map").contains_key(hash),
        }
    }

    pub fn get(&self, hash: &BlobHash) -> Result<Option<Vec<u8>>> {
        match &self.0 {
            Inner::Disk(dir) => match std::fs::read(dir.join(hash.to_string())) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e).context("reading a blob"),
            },
            Inner::Memory(map) => Ok(map.lock().expect("blob map").get(hash).map(|b| b.to_vec())),
        }
    }

    /// Stores `bytes` if they hash to `hash`. False when they do not.
    pub fn put(&self, hash: &BlobHash, bytes: &[u8]) -> Result<bool> {
        if blob_hash(bytes) != *hash {
            return Ok(false);
        }
        match &self.0 {
            Inner::Disk(dir) => {
                let path = dir.join(hash.to_string());
                if !path.is_file() {
                    // Written aside, on disk, then renamed: a blob is either whole or not there,
                    // also after a crash. Each upload has a file of its own, as two of the same
                    // blob can arrive at once.
                    static UPLOADS: AtomicU64 = AtomicU64::new(0);
                    let n = UPLOADS.fetch_add(1, Ordering::Relaxed);
                    let part = dir.join(format!("{hash}.{n}.part"));
                    let write = || -> std::io::Result<()> {
                        let mut file = std::fs::File::create(&part)?;
                        file.write_all(bytes)?;
                        file.sync_all()
                    };
                    if let Err(err) = write() {
                        let _ = std::fs::remove_file(&part);
                        return Err(err).context("writing a blob");
                    }
                    std::fs::rename(&part, &path).context("storing a blob")?;
                }
            }
            Inner::Memory(map) => {
                map.lock().expect("blob map").insert(*hash, bytes.into());
            }
        }
        Ok(true)
    }
}
