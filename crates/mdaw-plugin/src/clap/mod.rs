//! The CLAP backend, on `clack-host`.

mod editor;
mod handlers;
mod instance;
mod processor;

use crate::{Backend, Instance, PluginError, PluginFormat, PluginInfo, Settings, error};
use clack_host::prelude::*;
use instance::ClapInstance;
use std::path::{Path, PathBuf};

pub struct ClapBackend {
    /// The plugin files to load: `None` for the usual places.
    files: Option<Vec<PathBuf>>,
    /// Include the built-in test synth.
    builtin: bool,
    /// Every file loaded, with what it holds.
    entries: Vec<(PluginEntry, Vec<PluginInfo>)>,
}

/// Where the built-in test synth says it is.
pub const BUILTIN: &str = "built in";

impl ClapBackend {
    /// The plugins installed in the usual places, and the test synth if `builtin`.
    pub fn standard(builtin: bool) -> Self {
        Self {
            files: None,
            builtin,
            entries: Vec::new(),
        }
    }

    pub fn test_synth_only() -> Self {
        Self::files(Vec::new(), true)
    }

    /// Only these files, and the test synth if `builtin`.
    pub fn files(files: Vec<PathBuf>, builtin: bool) -> Self {
        Self {
            files: Some(files),
            builtin,
            entries: Vec::new(),
        }
    }
}

/// Every CLAP file in the usual places.
pub fn plugin_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    for dir in search_paths() {
        collect(&dir, &mut files, 0);
    }
    files.sort();
    files.dedup();
    files
}

/// The folders CLAP plugins are installed in, after the CLAP specification, and `CLAP_PATH`.
fn search_paths() -> Vec<PathBuf> {
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

fn collect(dir: &Path, out: &mut Vec<PathBuf>, depth: u32) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "clap") && path.is_file() {
            out.push(path);
        } else if path.is_dir() && depth < 4 {
            collect(&path, out, depth + 1);
        }
    }
}

fn lossy(s: Option<&std::ffi::CStr>) -> String {
    s.map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

fn describe(entry: &PluginEntry, location: &str) -> Vec<PluginInfo> {
    let Some(factory) = entry.get_plugin_factory() else {
        return Vec::new();
    };
    factory
        .plugin_descriptors()
        .filter_map(|d| {
            let id = lossy(d.id());
            if id.is_empty() {
                return None;
            }
            let instrument = d.features().any(|f| f.to_bytes() == b"instrument");
            Some(PluginInfo {
                format: PluginFormat::Clap,
                name: Some(lossy(d.name()))
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| id.clone()),
                vendor: lossy(d.vendor()),
                id,
                instrument,
                location: location.to_string(),
            })
        })
        .collect()
}

impl Backend for ClapBackend {
    fn format(&self) -> PluginFormat {
        PluginFormat::Clap
    }

    fn scan(&mut self) -> Vec<PluginInfo> {
        self.entries.clear();
        // The test synth comes with the program, in-process.
        if self.builtin
            && let Ok(entry) =
                PluginEntry::load_from_clack::<mdaw_test_plugin::Entry>(c"mdaw-test-synth.clap")
        {
            let found = describe(&entry, BUILTIN);
            self.entries.push((entry, found));
        }
        {
            let files = self.files.clone().unwrap_or_else(plugin_files);
            for file in files {
                // SAFETY: loading a CLAP file runs its initialisers. A host can do no better than
                // trust the plugins the user installed.
                let Ok(entry) = (unsafe { PluginEntry::load(&file) }) else {
                    continue;
                };
                let found = describe(&entry, &file.display().to_string());
                self.entries.push((entry, found));
            }
        }
        // The first of two plugins with one id wins.
        let mut seen = std::collections::HashSet::new();
        for (_, found) in &mut self.entries {
            found.retain(|p| seen.insert(p.id.clone()));
        }
        self.entries.iter().flat_map(|(_, found)| found.clone()).collect()
    }

    fn create(&mut self, id: &str, settings: Settings) -> Result<Box<dyn Instance>, PluginError> {
        let (entry, info) = self
            .entries
            .iter()
            .find_map(|(entry, found)| Some((entry, found.iter().find(|p| p.id == id)?)))
            .ok_or_else(|| error(format!("CLAP plugin {id} is not installed")))?;
        Ok(Box::new(ClapInstance::new(entry, info.clone(), settings)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_test_synth_is_offered_only_when_asked_for() {
        let without = ClapBackend::files(Vec::new(), false).scan();
        assert!(without.is_empty());
        let with = ClapBackend::files(Vec::new(), true).scan();
        assert!(with.iter().any(|p| p.id == mdaw_test_plugin::ID));
        // The usual places: whatever is installed, but no test synth unless asked for.
        let standard = ClapBackend::standard(false);
        assert!(standard.files.is_none() && !standard.builtin);
    }
}
