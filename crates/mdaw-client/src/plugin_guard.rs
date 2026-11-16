//! Remembers which plugins were being started when the client last ended, so that one that
//! takes the client down is not started again by itself (ADR 0002, failure policy, item 2).
//!
//! Two text files in the engine's settings folder, one plugin per line as
//! `<format name><TAB><plugin id>`:
//!
//! - `plugins-starting`: the plugins being started right now. Written (and forced to the
//!   disk) before a plugin is started, removed once the started plugins have run for a while.
//!   A client that ends in between leaves it behind.
//! - `plugins-off`: the plugins switched off because an earlier run left them in
//!   `plugins-starting`. They stay off until the user switches them on again.
//!
//! All of this is about this computer only; none of it is in the document.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use mdaw_model::PluginFormat;

/// A plugin as the document names it: its format and its own id in that format.
pub type PluginKey = (PluginFormat, String);

const STARTING: &str = "plugins-starting";
const OFF: &str = "plugins-off";

pub struct PluginGuard {
    dir: PathBuf,
    starting: BTreeSet<PluginKey>,
    off: BTreeSet<PluginKey>,
}

impl PluginGuard {
    /// Reads the two files in `dir` (created if missing). Plugins found in `plugins-starting`
    /// were being started when the client last ended: they are added to the switched-off
    /// ones. Returns the guard and the plugins switched off just now.
    pub fn open(dir: &Path) -> io::Result<(Self, Vec<PluginKey>)> {
        std::fs::create_dir_all(dir)?;
        let left = read(&dir.join(STARTING))?;
        let mut off = read(&dir.join(OFF))?;
        let newly: Vec<PluginKey> = left.into_iter().filter(|key| off.insert(key.clone())).collect();
        if !newly.is_empty() {
            write(dir, OFF, &off)?;
        }
        // After `plugins-off` is on the disk: a crash between the two loses nothing.
        write(dir, STARTING, &BTreeSet::new())?;
        let guard = Self {
            dir: dir.to_path_buf(),
            starting: BTreeSet::new(),
            off,
        };
        Ok((guard, newly))
    }

    pub fn is_off(&self, format: PluginFormat, id: &str) -> bool {
        self.off.iter().any(|(f, i)| *f == format && i == id)
    }

    /// Call before starting these plugins: when this returns, they are in `plugins-starting`
    /// on the disk. Nothing is written when all of them are in it already. A plugin whose id
    /// holds a tab or a line break cannot be written and is left out.
    pub fn starting(&mut self, plugins: impl IntoIterator<Item = PluginKey>) -> io::Result<()> {
        let mut added = false;
        for key in plugins {
            if !key.1.contains(['\t', '\n', '\r']) {
                added |= self.starting.insert(key);
            }
        }
        if added {
            write(&self.dir, STARTING, &self.starting)?;
        }
        Ok(())
    }

    /// The plugins started have run for a while: removes `plugins-starting`.
    pub fn settled(&mut self) -> io::Result<()> {
        if self.starting.is_empty() {
            return Ok(());
        }
        self.starting.clear();
        write(&self.dir, STARTING, &self.starting)
    }

    /// Switches a plugin on again.
    pub fn switch_on(&mut self, format: PluginFormat, id: &str) -> io::Result<()> {
        let before = self.off.len();
        self.off.retain(|(f, i)| !(*f == format && i == id));
        if self.off.len() != before {
            write(&self.dir, OFF, &self.off)?;
        }
        Ok(())
    }

    /// The client ends in the normal way: plugins still in their first moments are not to
    /// blame for that. Errors are ignored; the worst is a plugin switched off wrongly.
    pub fn ended_normally(dir: &Path) {
        let _ = std::fs::remove_file(dir.join(STARTING));
    }
}

/// The plugins a file lists. A missing file lists none; lines that do not parse are skipped.
fn read(path: &Path) -> io::Result<BTreeSet<PluginKey>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(err) => return Err(err),
    };
    Ok(String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| {
            let (format, id) = line.split_once('\t')?;
            let format = PluginFormat::from_name(format)?;
            (!id.is_empty()).then(|| (format, id.to_string()))
        })
        .collect())
}

/// Writes `set` as the file `name` in `dir`, whole: to the side, forced to the disk, then
/// renamed over the old one. An empty set removes the file.
fn write(dir: &Path, name: &str, set: &BTreeSet<PluginKey>) -> io::Result<()> {
    let path = dir.join(name);
    if set.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
            _ => Ok(()),
        };
    }
    let mut text = String::new();
    for (format, id) in set {
        text.push_str(format.name());
        text.push('\t');
        text.push_str(id);
        text.push('\n');
    }
    let part = dir.join(format!("{name}.part"));
    let mut file = File::create(&part)?;
    file.write_all(text.as_bytes())?;
    file.sync_data()?;
    std::fs::rename(&part, &path)?;
    File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synth() -> PluginKey {
        (PluginFormat::Clap, "org.example.synth".to_string())
    }

    fn other() -> PluginKey {
        (PluginFormat::Vst3, "ABCDEF".to_string())
    }

    #[test]
    fn nothing_there() {
        let dir = tempfile::tempdir().unwrap();
        let (guard, newly) = PluginGuard::open(dir.path()).unwrap();
        assert!(newly.is_empty());
        let (format, id) = synth();
        assert!(!guard.is_off(format, &id));
        assert!(!dir.path().join(STARTING).exists());
        assert!(!dir.path().join(OFF).exists());
    }

    #[test]
    fn a_start_that_never_settled_switches_off() {
        let dir = tempfile::tempdir().unwrap();
        let (mut guard, _) = PluginGuard::open(dir.path()).unwrap();
        guard.starting([synth()]).unwrap();
        drop(guard);
        let (guard, newly) = PluginGuard::open(dir.path()).unwrap();
        assert_eq!(newly, vec![synth()]);
        let (format, id) = synth();
        assert!(guard.is_off(format, &id));
        assert!(!dir.path().join(STARTING).exists());
        assert!(dir.path().join(OFF).exists());
        drop(guard);
        let (guard, newly) = PluginGuard::open(dir.path()).unwrap();
        assert!(newly.is_empty());
        assert!(guard.is_off(format, &id));
    }

    #[test]
    fn a_settled_start_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (mut guard, _) = PluginGuard::open(dir.path()).unwrap();
        guard.starting([synth()]).unwrap();
        guard.settled().unwrap();
        drop(guard);
        let (_, newly) = PluginGuard::open(dir.path()).unwrap();
        assert!(newly.is_empty());
        assert!(!dir.path().join(STARTING).exists());
    }

    #[test]
    fn switching_on_again() {
        let dir = tempfile::tempdir().unwrap();
        let (mut guard, _) = PluginGuard::open(dir.path()).unwrap();
        guard.starting([synth(), other()]).unwrap();
        drop(guard);
        let (mut guard, newly) = PluginGuard::open(dir.path()).unwrap();
        assert_eq!(newly.len(), 2);
        let (format, id) = synth();
        guard.switch_on(format, &id).unwrap();
        drop(guard);
        let (guard, newly) = PluginGuard::open(dir.path()).unwrap();
        assert!(!guard.is_off(format, &id));
        let (format, id) = other();
        assert!(guard.is_off(format, &id));
        assert!(newly.is_empty());
    }

    #[test]
    fn the_last_one_switched_on_removes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let (mut guard, _) = PluginGuard::open(dir.path()).unwrap();
        guard.starting([synth()]).unwrap();
        drop(guard);
        let (mut guard, _) = PluginGuard::open(dir.path()).unwrap();
        assert!(dir.path().join(OFF).exists());
        let (format, id) = synth();
        guard.switch_on(format, &id).unwrap();
        assert!(!dir.path().join(OFF).exists());
    }

    #[test]
    fn lines_that_do_not_parse_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(OFF),
            "CLAP\torg.example.synth\ngarbage\nNOPE\tx\nVST3\tABCDEF\n",
        )
        .unwrap();
        let (guard, newly) = PluginGuard::open(dir.path()).unwrap();
        assert!(newly.is_empty());
        assert_eq!(guard.off, BTreeSet::from([synth(), other()]));
    }

    #[test]
    fn ended_normally_forgets_the_start() {
        let dir = tempfile::tempdir().unwrap();
        let (mut guard, _) = PluginGuard::open(dir.path()).unwrap();
        guard.starting([synth()]).unwrap();
        PluginGuard::ended_normally(dir.path());
        drop(guard);
        let (_, newly) = PluginGuard::open(dir.path()).unwrap();
        assert!(newly.is_empty());
    }
}
