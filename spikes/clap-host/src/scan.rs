//! Plugin discovery: standard CLAP search paths for Linux and Windows.

use clack_host::prelude::*;
use std::path::{Path, PathBuf};

pub struct Found {
    pub file: PathBuf,
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub version: String,
    pub features: Vec<String>,
}

pub fn search_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    #[cfg(unix)]
    {
        if let Some(home) = std::env::var_os("HOME") {
            paths.push(Path::new(&home).join(".clap"));
        }
        paths.push("/usr/lib/clap".into());
        paths.push("/usr/local/lib/clap".into());
    }
    #[cfg(windows)]
    {
        if let Some(dir) = std::env::var_os("COMMONPROGRAMFILES") {
            paths.push(Path::new(&dir).join("CLAP"));
        }
        if let Some(dir) = std::env::var_os("LOCALAPPDATA") {
            paths.push(Path::new(&dir).join("Programs").join("Common").join("CLAP"));
        }
    }
    if let Some(extra) = std::env::var_os("CLAP_PATH") {
        paths.extend(std::env::split_paths(&extra));
    }
    paths
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "clap") && path.is_file() {
            out.push(path);
        } else if path.is_dir() {
            collect(&path, out);
        }
    }
}

pub fn plugin_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    for dir in search_paths() {
        collect(&dir, &mut files);
    }
    files.sort();
    files
}

fn lossy(s: Option<&std::ffi::CStr>) -> String {
    s.map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Loads one plugin file and lists what it contains.
///
/// This runs plugin code in-process; a real scanner belongs in a child process.
pub fn describe(file: &Path) -> Result<(PluginEntry, Vec<Found>), String> {
    // SAFETY: loading a CLAP file runs its initialisers; that is the point of a host.
    let entry = unsafe { PluginEntry::load(file) }.map_err(|e| e.to_string())?;
    let factory = entry.get_plugin_factory().ok_or("no plugin factory")?;
    let found = factory
        .plugin_descriptors()
        .map(|d| Found {
            file: file.to_path_buf(),
            id: lossy(d.id()),
            name: lossy(d.name()),
            vendor: lossy(d.vendor()),
            version: lossy(d.version()),
            features: d.features().map(|f| f.to_string_lossy().into_owned()).collect(),
        })
        .collect();
    Ok((entry, found))
}

/// Finds the file that provides `plugin_id`.
pub fn find(plugin_id: &str) -> Result<(PluginEntry, Found), String> {
    for file in plugin_files() {
        if let Ok((entry, found)) = describe(&file) {
            if let Some(f) = found.into_iter().find(|f| f.id == plugin_id) {
                return Ok((entry, f));
            }
        }
    }
    Err(format!(
        "plugin id {plugin_id:?} not found in {:?}",
        search_paths()
    ))
}
