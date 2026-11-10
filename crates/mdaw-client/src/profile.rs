//! What a client install remembers about its user: a random key the server recognises them by,
//! and a display name.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::link::Who;

const KEY_FILE: &str = "user.key";
const NAME_FILE: &str = "name";

/// The profile directory used when none is given.
pub fn default_dir() -> Option<PathBuf> {
    let var = |name: &str| {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let base = if cfg!(windows) {
        var("APPDATA")
    } else if cfg!(target_os = "macos") {
        var("HOME").map(|home| home.join("Library/Application Support"))
    } else {
        var("XDG_CONFIG_HOME").or_else(|| var("HOME").map(|home| home.join(".config")))
    };
    base.map(|base| base.join("mdaw"))
}

/// Reads the profile in `dir`, creating what is missing. A `name` given here replaces the
/// stored one.
pub fn open(dir: &Path, name: Option<&str>) -> Result<Who> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

    let key_path = dir.join(KEY_FILE);
    let user_key = match std::fs::read_to_string(&key_path) {
        Ok(text) => parse_key(text.trim()).with_context(|| format!("{} is damaged", key_path.display()))?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let mut key = [0u8; 16];
            getrandom::fill(&mut key).map_err(|err| anyhow::anyhow!("no random numbers: {err}"))?;
            let hex: String = key.iter().map(|byte| format!("{byte:02x}")).collect();
            std::fs::write(&key_path, hex).with_context(|| format!("writing {}", key_path.display()))?;
            key
        }
        Err(err) => return Err(err).with_context(|| format!("reading {}", key_path.display())),
    };

    let name_path = dir.join(NAME_FILE);
    let name = match name.map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) => {
            std::fs::write(&name_path, name).with_context(|| format!("writing {}", name_path.display()))?;
            name.to_string()
        }
        None => match std::fs::read_to_string(&name_path) {
            Ok(stored) if !stored.trim().is_empty() => stored.trim().to_string(),
            _ => login_name(),
        },
    };
    Ok(Who { user_key, name })
}

fn parse_key(hex: &str) -> Option<[u8; 16]> {
    if hex.len() != 32 || !hex.is_ascii() {
        return None;
    }
    let mut key = [0u8; 16];
    for (byte, pair) in key.iter_mut().zip(hex.as_bytes().chunks(2)) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(key)
}

fn login_name() -> String {
    ["USER", "USERNAME"]
        .iter()
        .filter_map(|var| std::env::var(var).ok())
        .find(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "anonymous".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_profile_keeps_its_key_and_name() {
        let dir = tempfile::tempdir().unwrap();
        let first = open(dir.path(), Some("  ann ")).unwrap();
        assert_eq!(first.name, "ann");
        let again = open(dir.path(), None).unwrap();
        assert_eq!((again.user_key, again.name.as_str()), (first.user_key, "ann"));
        let renamed = open(dir.path(), Some("anna")).unwrap();
        assert_eq!(
            (renamed.user_key, renamed.name.as_str()),
            (first.user_key, "anna")
        );

        let other = tempfile::tempdir().unwrap();
        assert_ne!(open(other.path(), None).unwrap().user_key, first.user_key);
    }

    #[test]
    fn a_damaged_key_is_an_error_not_a_new_identity() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(KEY_FILE), "not a key").unwrap();
        assert!(open(dir.path(), None).is_err());
    }
}
