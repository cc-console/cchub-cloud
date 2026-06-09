//! Cross-platform path helpers.

use std::path::PathBuf;

/// The user's home directory. `$HOME` on unix; `%USERPROFILE%` (falling back to
/// `%HOMEDRIVE%%HOMEPATH%`) on Windows, where `HOME` is usually unset. Returns
/// `None` only if none of those are set.
pub fn home_dir() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("HOME") {
        return Some(PathBuf::from(h));
    }
    if let Some(up) = std::env::var_os("USERPROFILE") {
        return Some(PathBuf::from(up));
    }
    match (std::env::var_os("HOMEDRIVE"), std::env::var_os("HOMEPATH")) {
        (Some(drive), Some(path)) => {
            let mut p = PathBuf::from(drive);
            p.push(path);
            Some(p)
        }
        _ => None,
    }
}
