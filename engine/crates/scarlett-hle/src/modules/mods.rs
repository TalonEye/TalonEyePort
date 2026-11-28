//! Mods laid over a virtual drive, loaded as they are.
//!
//! A mods folder holds one folder per mod, each laid out like the drive it
//! covers: `mods/<name>/x64/a.rpf` stands in for `G:\x64\a.rpf`. When mods
//! are on, [`install`] walks every mod folder once, in folder-name order,
//! and indexes each file by its path on the drive; a later folder wins over
//! an earlier one. From then on, a title path on that drive that is in the
//! index opens the mod's file instead of the dump's. Nothing is converted,
//! copied or written: the files are opened as the mod ships them, and only
//! for reading ([`super::files`] asks for the overlay only then).
//!
//! A file under a folder named like an archive on the drive
//! (`mods/<name>/common.rpf/data/version.txt`) replaces that entry inside the
//! archive instead: [`super::archives`] points the entry at the mod's file
//! when the title reads the archive's table.
//!
//! The index is built once and never changes during a run.

use std::collections::HashMap;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering::Relaxed};

/// The overlay: a mod file per normalised drive path, and the mods' names.
struct Overlay {
    files: HashMap<String, Entry>,
    names: Vec<String>,
}

/// A mod file standing in for a drive path.
struct Entry {
    /// The host file: UTF-16, absolute, no NUL.
    host: Vec<u16>,
    /// The mod it comes from, as an index into [`Overlay::names`].
    index: usize,
    /// Whether the title has asked for it yet (it is logged the first time).
    used: AtomicBool,
}

static OVERLAY: RwLock<Option<Overlay>> = RwLock::new(None);

/// The drive letter (upper case) the overlay covers; 0 = no overlay. Read
/// before taking the lock, so a run without mods pays one atomic load.
static LETTER: AtomicU16 = AtomicU16::new(0);

/// What [`install`] found.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Summary {
    /// Each mod folder, in load order, with the files it ships.
    pub mods: Vec<(String, usize)>,
    /// Files that stand in for one the drive already has.
    pub replaced: usize,
    /// Files the drive did not have.
    pub added: usize,
    /// Paths shipped by more than one mod: the path, and the mods that ship
    /// it in load order (the last one wins).
    pub conflicts: Vec<(String, Vec<String>)>,
    /// Files that replace entries inside archives, by archive.
    pub archives: super::archives::Planned,
}

/// Where `key` (a drive path) lies inside an archive: the archive's drive
/// path and the path inside it (lower case, `/`), split at the first folder
/// named like an archive.
fn in_archive(key: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = key.split('\\').collect();
    let at = parts[..parts.len().saturating_sub(1)].iter().position(|p| p.ends_with(".RPF"))?;
    let inner = parts[at + 1..].join("/").to_lowercase();
    Some((parts[..=at].join("\\"), inner))
}

/// Lays the mods in `dir` over drive `letter`, whose host folder is `root`
/// (to tell replaced files from added ones). A missing `dir` is no mods.
/// Replaces an earlier overlay.
pub fn install(letter: char, root: &Path, dir: &Path) -> std::io::Result<Summary> {
    let mut folders: Vec<(String, PathBuf)> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e),
    };
    // Folder-name order, ignoring case, so `10-a` loads before `20-b`.
    folders.sort_by(|a, b| {
        a.0.to_lowercase()
            .cmp(&b.0.to_lowercase())
            .then_with(|| a.0.cmp(&b.0))
    });

    let mut files: HashMap<String, Entry> = HashMap::new();
    let mut shipped: HashMap<String, Vec<usize>> = HashMap::new();
    let mut summary = Summary::default();
    let mut names = Vec::with_capacity(folders.len());
    // Archive, then path inside it, to the mod and its file; the last mod wins.
    let mut archived: HashMap<String, HashMap<String, (usize, PathBuf)>> = HashMap::new();
    for (index, (name, path)) in folders.iter().enumerate() {
        let mut found = Vec::new();
        walk(path, &mut Vec::new(), &mut found)?;
        summary.mods.push((name.clone(), found.len()));
        names.push(name.clone());
        for (key, host) in found {
            let host = std::path::absolute(&host).unwrap_or(host);
            shipped.entry(key.clone()).or_default().push(index);
            if let Some((archive, inner)) = in_archive(&key) {
                archived.entry(archive).or_default().insert(inner, (index, host));
                continue;
            }
            let entry = Entry {
                host: host.as_os_str().encode_wide().collect(),
                index,
                used: AtomicBool::new(false),
            };
            files.insert(key, entry);
        }
    }
    for key in files.keys() {
        let on_drive = root.join(key.replace('\\', std::path::MAIN_SEPARATOR_STR));
        if on_drive.is_file() {
            summary.replaced += 1;
        } else {
            summary.added += 1;
        }
    }
    let mut conflicts: Vec<(String, Vec<String>)> = shipped
        .into_iter()
        .filter(|(_, by)| by.len() > 1)
        .map(|(key, by)| (key, by.into_iter().map(|i| names[i].clone()).collect()))
        .collect();
    conflicts.sort();
    summary.conflicts = conflicts;
    let mut wanted: Vec<(String, Vec<(String, String, PathBuf)>)> = archived
        .into_iter()
        .map(|(archive, files)| {
            let mut files: Vec<_> = files
                .into_iter()
                .map(|(inner, (index, host))| (inner, names[index].clone(), host))
                .collect();
            files.sort();
            (archive, files)
        })
        .collect();
    wanted.sort();
    // Nothing to plan leaves the archives alone (no hook is installed for them).
    if !wanted.is_empty() || super::archives::any() {
        // A mod may ship the whole archive too; the title then opens that copy.
        let file_for = |key: &str| match files.get(key) {
            Some(entry) => PathBuf::from(String::from_utf16_lossy(&entry.host)),
            None => root.join(key.replace('\\', std::path::MAIN_SEPARATOR_STR)),
        };
        summary.archives = super::archives::set(file_for, wanted);
    }

    let letter = letter.to_ascii_uppercase() as u16;
    let active = !files.is_empty();
    *OVERLAY.write().unwrap_or_else(|e| e.into_inner()) = Some(Overlay { files, names });
    LETTER.store(if active { letter } else { 0 }, Relaxed);
    Ok(summary)
}

/// Logs what [`install`] found: a line per mod, conflicts, and a total.
pub fn log(summary: &Summary, dir: &Path) {
    for (name, count) in &summary.mods {
        tracing::info!(name = %name, files = count, "mod");
    }
    for (path, by) in &summary.conflicts {
        tracing::warn!(
            path = %path,
            mods = %by.join(", "),
            wins = %by.last().map_or("", String::as_str),
            "more than one mod ships this file; the last one wins"
        );
    }
    for (archive, files) in &summary.archives.archives {
        tracing::info!(archive = %archive, files, "mod files replace entries inside an archive");
    }
    for archive in &summary.archives.missing {
        tracing::warn!(archive = %archive, "mod files are in a folder named like an archive the package does not have; they are not used");
    }
    tracing::info!(
        dir = %dir.display(),
        mods = summary.mods.len(),
        replaced = summary.replaced,
        added = summary.added,
        in_archives = summary.archives.archives.iter().map(|(_, n)| n).sum::<usize>(),
        conflicts = summary.conflicts.len(),
        "mods laid over the package"
    );
}

/// Every file under `dir`, as (normalised drive path, host path). `rel` is
/// the folders walked so far.
fn walk(
    dir: &Path,
    rel: &mut Vec<String>,
    out: &mut Vec<(String, PathBuf)>,
) -> std::io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(Result::ok).collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        rel.push(entry.file_name().to_string_lossy().to_uppercase());
        if kind.is_dir() {
            walk(&entry.path(), rel, out)?;
        } else if kind.is_file() || kind.is_symlink() {
            out.push((rel.join("\\"), entry.path()));
        }
        rel.pop();
    }
    Ok(())
}

/// `tail` (a path after `X:`) as an index key: separators unified, `.`
/// dropped, `..` resolved (stopping at the root), upper case.
fn key(tail: &[u16]) -> String {
    let text = String::from_utf16_lossy(tail);
    let mut parts: Vec<&str> = Vec::new();
    for part in text.split(['\\', '/']) {
        match part {
            "" | "." => {}
            ".." => drop(parts.pop()),
            _ => parts.push(part),
        }
    }
    parts.join("\\").to_uppercase()
}

/// The mod file (UTF-16, absolute, no NUL) standing in for `tail` on drive
/// `letter` (upper case), and the mod's name. The first time the title asks
/// for each file, that is logged at info level.
pub(crate) fn lookup(letter: u16, tail: &[u16]) -> Option<(Vec<u16>, String)> {
    if letter == 0 || LETTER.load(Relaxed) != letter {
        return None;
    }
    let overlay = OVERLAY.read().unwrap_or_else(|e| e.into_inner());
    let overlay = overlay.as_ref()?;
    let key = key(tail);
    let entry = overlay.files.get(&key)?;
    let name = &overlay.names[entry.index];
    if !entry.used.swap(true, Relaxed) {
        tracing::info!(path = %key, r#mod = %name, "title reads a file from a mod");
    }
    Some((entry.host.clone(), name.clone()))
}

/// Removes the overlay (for tests).
#[cfg(test)]
pub(crate) fn clear() {
    LETTER.store(0, Relaxed);
    *OVERLAY.write().unwrap_or_else(|e| e.into_inner()) = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn keys_ignore_case_separators_and_dots() {
        assert_eq!(key(&w(r"\x64\a.rpf")), r"X64\A.RPF");
        assert_eq!(key(&w("/x64//./b/../A.rpf")), r"X64\A.RPF");
        assert_eq!(key(&w(r"..\..\a.rpf")), "A.RPF");
        assert_eq!(key(&w("")), "");
    }

    #[test]
    fn files_in_archive_folders_split_at_the_archive() {
        assert_eq!(
            in_archive(r"COMMON.RPF\DATA\VERSION.TXT"),
            Some(("COMMON.RPF".into(), "data/version.txt".into()))
        );
        assert_eq!(
            in_archive(r"X64\LEVELS.RPF\A\B.YMAP"),
            Some((r"X64\LEVELS.RPF".into(), "a/b.ymap".into()))
        );
        assert_eq!(in_archive(r"X64\A.RPF"), None);
        assert_eq!(in_archive("TOP.DAT"), None);
    }

    #[test]
    fn walk_lists_files_with_drive_keys() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("x64/audio")).unwrap();
        std::fs::write(dir.path().join("x64/audio/Music.rpf"), b"m").unwrap();
        std::fs::write(dir.path().join("top.dat"), b"t").unwrap();
        let mut out = Vec::new();
        walk(dir.path(), &mut Vec::new(), &mut out).unwrap();
        let keys: Vec<&str> = out.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["TOP.DAT", r"X64\AUDIO\MUSIC.RPF"]);
    }
}
