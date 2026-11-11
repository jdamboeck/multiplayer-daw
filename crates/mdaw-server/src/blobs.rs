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
        let mut writer = self.writer(*hash)?;
        writer.write(bytes)?;
        writer.finish()
    }

    /// Takes in a blob a piece at a time, as it arrives, without holding it in memory.
    pub fn writer(&self, hash: BlobHash) -> Result<BlobWriter> {
        let sink = match &self.0 {
            Inner::Disk(dir) => {
                // Written aside, on disk, then renamed: a blob is either whole or not there,
                // also after a crash. Each upload has a file of its own, as two of the same blob
                // can arrive at once.
                static UPLOADS: AtomicU64 = AtomicU64::new(0);
                let n = UPLOADS.fetch_add(1, Ordering::Relaxed);
                let part = dir.join(format!("{hash}.{n}.part"));
                let file = std::fs::File::create(&part).context("writing a blob")?;
                Sink::File {
                    dir: dir.clone(),
                    part,
                    file,
                }
            }
            Inner::Memory(map) => Sink::Memory(map.clone(), Vec::new()),
        };
        Ok(BlobWriter {
            hash,
            hasher: blake3::Hasher::new(),
            sink: Some(sink),
        })
    }
}

enum Sink {
    File {
        dir: PathBuf,
        part: PathBuf,
        file: std::fs::File,
    },
    Memory(Arc<Mutex<HashMap<BlobHash, Arc<[u8]>>>>, Vec<u8>),
}

/// A blob on its way into the store. Dropped unfinished, it leaves nothing behind.
pub struct BlobWriter {
    hash: BlobHash,
    hasher: blake3::Hasher,
    sink: Option<Sink>,
}

impl BlobWriter {
    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.hasher.update(bytes);
        match self.sink.as_mut() {
            Some(Sink::File { file, .. }) => file.write_all(bytes).context("writing a blob"),
            Some(Sink::Memory(_, buffer)) => {
                buffer.extend_from_slice(bytes);
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// Stores the blob if what was written hashes to what it was said to be. False if not.
    pub fn finish(mut self) -> Result<bool> {
        if BlobHash(*self.hasher.finalize().as_bytes()) != self.hash {
            return Ok(false);
        }
        match self.sink.take() {
            Some(Sink::File { dir, part, file }) => {
                if let Err(err) = file.sync_all() {
                    let _ = std::fs::remove_file(&part);
                    return Err(err).context("writing a blob");
                }
                let path = dir.join(self.hash.to_string());
                std::fs::rename(&part, &path).context("storing a blob")?;
                // The new name is on disk too before the upload is answered `Stored`.
                #[cfg(unix)]
                std::fs::File::open(&dir)
                    .and_then(|dir| dir.sync_all())
                    .context("storing a blob")?;
            }
            Some(Sink::Memory(map, buffer)) => {
                map.lock().expect("blob map").insert(self.hash, buffer.into());
            }
            None => {}
        }
        Ok(true)
    }
}

impl Drop for BlobWriter {
    fn drop(&mut self) {
        if let Some(Sink::File { part, .. }) = self.sink.take() {
            let _ = std::fs::remove_file(part);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blob_written_in_pieces_is_stored_only_if_it_is_what_it_said() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = Blobs::open(dir.path()).unwrap();
        let bytes: Vec<u8> = (0..300_000u32).map(|i| (i * 7) as u8).collect();
        let hash = blob_hash(&bytes);

        let mut writer = blobs.writer(hash).unwrap();
        writer.write(&bytes[..1000]).unwrap();
        writer.write(b"something else").unwrap();
        assert!(!writer.finish().unwrap(), "not what it said");
        assert!(!blobs.has(&hash));

        let mut writer = blobs.writer(hash).unwrap();
        for piece in bytes.chunks(65_536) {
            writer.write(piece).unwrap();
        }
        assert!(writer.finish().unwrap());
        assert_eq!(blobs.get(&hash).unwrap().unwrap(), bytes);

        // One given up halfway leaves nothing behind.
        let writer = blobs.writer(blob_hash(b"x")).unwrap();
        drop(writer);
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "part"))
            .collect();
        assert!(left.is_empty());
    }
}
