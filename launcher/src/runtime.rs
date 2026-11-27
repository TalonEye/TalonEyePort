//! Where the engines live. Nothing is embedded in the launcher and nothing is unpacked or
//! downloaded at run time: the release ZIP puts a `runtime` folder beside ScarlettPort.exe
//! (ScarlettPort-Runtime.exe, the ScarlettPort runtime, and its `D3D12` folder for the World engine; xcl.exe, dxcompiler.dll and
//! dxil.dll for the Classic engine when present), and the launcher runs them from there.
//! `SCARLETT_RUNTIME_DIR` points at another folder (for development builds).
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Settings and the default cache location (data only, never executables).
pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("ScarlettPort")
}

/// The `runtime` folder beside the launcher.
pub fn runtime_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("SCARLETT_RUNTIME_DIR") {
        return PathBuf::from(d);
    }
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.join("runtime")))
        .unwrap_or_else(|| PathBuf::from("runtime"))
}

pub fn xcl_exe() -> PathBuf {
    runtime_dir().join("xcl.exe")
}

/// The release's name for the ScarlettPort runtime (scarlett.exe).
pub const RUNTIME_EXE: &str = "ScarlettPort-Runtime.exe";

/// The World engine: `ScarlettPort-Runtime.exe`, else a development build's `scarlett.exe`.
pub fn scarlett_exe() -> PathBuf {
    let dir = runtime_dir();
    let p = dir.join(RUNTIME_EXE);
    if p.is_file() {
        return p;
    }
    dir.join("scarlett.exe")
}

/// The former spelling of a `SCARLETT_*` switch, which the runtime still reads when the
/// `SCARLETT_*` one is unset.
fn legacy_name(key: &str) -> Option<String> {
    key.strip_prefix("SCARLETT_").map(|rest| format!("VICTORY_{rest}")) // rename:keep
}

/// Sets a runtime switch on `cmd` under its `SCARLETT_*` name.
pub fn set_switch(cmd: &mut std::process::Command, key: &str, value: &str) {
    cmd.env(key, value);
    // Local and older builds read some switches (the upscaler, watchdog, relay and pipeline
    // wait among them) only under the former spelling; the bundled runtime prefers the new
    // name, so setting both to the same value is safe.
    if let Some(old) = legacy_name(key) {
        cmd.env(old, value);
    }
}

/// Clears a runtime switch (also under its former spelling, which the runtime would read instead).
pub fn clear_switch(cmd: &mut std::process::Command, key: &str) {
    cmd.env_remove(key);
    if let Some(old) = legacy_name(key) {
        cmd.env_remove(old);
    }
}

/// The data folder's former name under `%LOCALAPPDATA%`.
const LEGACY_DATA_DIR: &str = "victory"; // rename:keep

/// On first start: what the former data folder holds (saves, caches, settings) and
/// `%LOCALAPPDATA%\ScarlettPort` does not is moved over (copied when it cannot be moved);
/// the launcher's own files in `%LOCALAPPDATA%\ScarlettPort` are left as they are.
pub fn migrate_legacy_data() {
    if let Some(base) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
        migrate_legacy_data_in(&base);
    }
}

/// `migrate_legacy_data` for the data folders under `base` (`%LOCALAPPDATA%`).
fn migrate_legacy_data_in(base: &Path) {
    let root = base.join("ScarlettPort");
    let old_toml = root.join(format!("{LEGACY_DATA_DIR}.toml"));
    if old_toml.is_file() && !root.join("scarlett.toml").exists() {
        let _ = std::fs::rename(&old_toml, root.join("scarlett.toml"));
    }
    let old = base.join(LEGACY_DATA_DIR);
    let Ok(entries) = std::fs::read_dir(&old) else { return };
    if std::fs::create_dir_all(&root).is_err() {
        return;
    }
    for entry in entries.flatten() {
        let target = root.join(entry.file_name());
        if target.exists() || std::fs::rename(entry.path(), &target).is_ok() {
            continue;
        }
        let _ = copy_tree(&entry.path(), &target);
    }
    let _ = std::fs::remove_dir(&old);
}

/// A folder setting that pointed into the former data folder (`<base>\<old name>\...`) points
/// into `<base>\ScarlettPort\...` after the move. Other paths are returned as they are.
pub fn migrate_data_path(path: &str, base: &Path) -> String {
    let old = base.join(LEGACY_DATA_DIR).display().to_string();
    let t = path.trim();
    let n = old.len();
    let inside = t.len() >= n && t.is_char_boundary(n) && t[..n].eq_ignore_ascii_case(&old) && (t.len() == n || t[n..].starts_with(['\\', '/']));
    if inside { format!("{}{}", base.join("ScarlettPort").display(), &t[n..]) } else { path.to_string() }
}

/// A shader cache folder kept under its former name inside `cache` takes the new one.
pub fn migrate_legacy_cache(cache: &Path, new: &Path) {
    let old = cache.join(LEGACY_DATA_DIR);
    if old.is_dir() && !new.exists() {
        let _ = std::fs::rename(&old, new);
    }
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    if !from.is_dir() {
        return std::fs::copy(from, to).map(|_| ());
    }
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        copy_tree(&entry.path(), &to.join(entry.file_name()))?;
    }
    Ok(())
}

/// What was found in the runtime folder: (folder, World engine hash, Classic engine hash).
/// A hash is the SHA-256 of the engine's executable; it is what "shaders built for" refers to.
pub struct Found {
    pub dir: PathBuf,
    pub world: Option<String>,
    pub classic: Option<String>,
}

pub fn file_hash(p: &Path) -> Option<String> {
    let bytes = std::fs::read(p).ok().filter(|b| !b.is_empty())?;
    Some(Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect())
}

/// Looks for the engines beside the launcher.
pub fn locate() -> Result<Found, String> {
    let dir = runtime_dir();
    let found = Found { world: file_hash(&scarlett_exe()), classic: file_hash(&xcl_exe()), dir: dir.clone() };
    if found.world.is_none() && found.classic.is_none() {
        return Err(format!(
            "No engine found in {}. Extract the whole release ZIP and keep the runtime folder beside ScarlettPort.exe.",
            dir.display()
        ));
    }
    Ok(found)
}

#[cfg(test)]
mod switch_tests {
    #[test]
    fn legacy_names() {
        assert!(super::legacy_name("SCARLETT_LOOK").is_some_and(|n| n.ends_with("_LOOK") && n != "SCARLETT_LOOK"));
        assert_eq!(super::legacy_name("XCL_DXC_DIR"), None);
    }

    #[test]
    fn data_paths_follow_the_moved_folder() {
        use std::path::Path;
        let base = Path::new(r"C:\Users\Zoë Ünal\AppData\Local");
        let old = base.join(super::LEGACY_DATA_DIR);
        let cache = format!(r"{}\cache", old.display());
        assert_eq!(super::migrate_data_path(&cache, base), format!(r"{}\cache", base.join("ScarlettPort").display()));
        let upper = format!(r"{}\cache", old.display().to_string().to_ascii_uppercase());
        assert_eq!(super::migrate_data_path(&upper, base), format!(r"{}\cache", base.join("ScarlettPort").display()));
        assert_eq!(super::migrate_data_path(r"D:\ScarlettCache", base), r"D:\ScarlettCache");
        let sibling = format!(r"{}2\x", old.display());
        assert_eq!(super::migrate_data_path(&sibling, base), sibling, "a folder that only starts with the old name stays");
        assert_eq!(super::migrate_data_path("", base), "");
    }

    #[test]
    fn legacy_data_moves_over_without_overwriting() {
        // A temporary %LOCALAPPDATA% with a space and a non-ASCII character in its name.
        let base = std::env::temp_dir().join(format!("sp-mig-{} ü", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let old = base.join(super::LEGACY_DATA_DIR);
        std::fs::create_dir_all(old.join("data")).unwrap();
        std::fs::write(old.join("data").join("save.bin"), "s").unwrap();
        std::fs::write(old.join("launcher.json"), "old").unwrap();
        std::fs::create_dir_all(base.join("ScarlettPort")).unwrap();
        std::fs::write(base.join("ScarlettPort").join("launcher.json"), "new").unwrap();
        super::migrate_legacy_data_in(&base);
        let new = base.join("ScarlettPort");
        assert_eq!(std::fs::read_to_string(new.join("data").join("save.bin")).unwrap(), "s");
        assert_eq!(std::fs::read_to_string(new.join("launcher.json")).unwrap(), "new", "existing files win");
        let _ = std::fs::remove_dir_all(&base);
    }
}
