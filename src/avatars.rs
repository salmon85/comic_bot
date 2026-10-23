//! PNG pool and avatar assignments.
//!
//! Users drop their own PNG files into the avatars directory. A chat command
//! (`!setavatar <user> <file>`) then binds one of those pictures to a chatter.
//! Assignments are stored in the config file, the pool itself is just the
//! directory content. The same scanner is used for the background pool.

use std::path::{Path, PathBuf};

/// A PNG that can be assigned to a chatter or used as a cell background.
#[derive(Debug, Clone)]
pub struct PoolEntry {
    pub path: PathBuf,
    /// File name without extension, used when assigning by name.
    pub stem: String,
}

/// All usable avatar files of the pool directory.
pub fn scan_pool(dir: &Path) -> Vec<PoolEntry> {
    let mut entries = Vec::new();
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return entries;
    };
    for item in read_dir.flatten() {
        let path = item.path();
        let is_png = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("png"))
            .unwrap_or(false);
        if !is_png || !path.is_file() {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("avatar")
            .to_string();
        entries.push(PoolEntry { path, stem });
    }
    entries.sort_by(|a, b| a.stem.to_lowercase().cmp(&b.stem.to_lowercase()));
    entries
}

/// Find a pool entry by file name or by name without extension.
pub fn find_in_pool<'a>(pool: &'a [PoolEntry], query: &str) -> Option<&'a PoolEntry> {
    let query = query.trim();
    let lower = query.to_lowercase();
    pool.iter().find(|entry| {
        entry.stem.to_lowercase() == lower
            || entry
                .path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.to_lowercase() == lower)
                .unwrap_or(false)
    })
}

/// Normalise a chat login so lookups are case insensitive.
pub fn normalize_login(login: &str) -> String {
    login.trim().trim_start_matches('@').to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logins_are_case_insensitive() {
        assert_eq!(normalize_login("@Salmon85"), "salmon85");
        assert_eq!(normalize_login("  Foo  "), "foo");
    }
}
