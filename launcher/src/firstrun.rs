//! First run: finding the game folder on its own, placing what the runtime needs, the player
//! name and the key that says when the shader cache must be prepared again.
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// How long the automatic search may take before it stops by itself.
pub const SEARCH_LIMIT: Duration = Duration::from_secs(25);

/// Folder names under a drive root (or a user folder) worth a deeper look.
pub const LIKELY: [&str; 10] = ["Games", "XboxGames", "Xbox Games", "Dumps", "Dump", "Xbox", "Series", "ScarlettPort", "Downloads", "Desktop"];

/// Folders never searched (system, caches, packages).
const SKIP: [&str; 14] = [
    "windows",
    "$recycle.bin",
    "system volume information",
    "programdata",
    "appdata",
    "recovery",
    "perflogs",
    "$windows.~bt",
    "$windows.~ws",
    "windowsapps",
    "node_modules",
    ".git",
    "target",
    "msocache",
];

fn skipped(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.starts_with('.') || SKIP.contains(&n.as_str())
}

/// Where to look and how deep: every drive root (2 levels), the likely folders on it
/// (3 levels below them), and the user's Downloads, Desktop and Documents (3 levels).
pub fn starts(drives: &[PathBuf], home: Option<&Path>) -> Vec<(PathBuf, u32)> {
    let mut out: Vec<(PathBuf, u32)> = Vec::new();
    if let Some(h) = home {
        for d in ["Downloads", "Desktop", "Documents"] {
            out.push((h.join(d), 3));
        }
    }
    for root in drives {
        for n in LIKELY {
            out.push((root.join(n), 3));
        }
        out.push((root.clone(), 2));
    }
    out
}

/// Breadth-first search of `starts` for folders holding `exe`, until `deadline` or `cancel`.
/// A found folder is not searched further. Returns true when the search ran to the end.
pub fn search(starts: &[(PathBuf, u32)], exe: &str, deadline: Instant, cancel: &AtomicBool, mut found: impl FnMut(PathBuf)) -> bool {
    let mut seen = std::collections::HashSet::new();
    let mut queue: VecDeque<(PathBuf, u32)> = starts.iter().cloned().collect();
    while let Some((dir, depth)) = queue.pop_front() {
        if cancel.load(Ordering::Relaxed) || Instant::now() >= deadline {
            return false;
        }
        let key = dir.to_string_lossy().to_lowercase();
        if !seen.insert(key) {
            continue;
        }
        if dir.join(exe).is_file() {
            found(dir);
            continue;
        }
        if depth == 0 {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            // file_type does not follow links: a junction loop is never walked
            if e.file_type().is_ok_and(|t| t.is_dir()) && !skipped(&e.file_name().to_string_lossy()) {
                queue.push_back((e.path(), depth - 1));
            }
        }
    }
    true
}

/// Fixed drives (`C:\`, `D:\`, ...).
pub fn fixed_drives() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        extern "system" {
            fn GetLogicalDrives() -> u32;
            fn GetDriveTypeW(root: *const u16) -> u32;
        }
        let mask = unsafe { GetLogicalDrives() };
        (0..26u8)
            .filter(|i| mask & (1 << i) != 0)
            .map(|i| format!("{}:\\", (b'A' + i) as char))
            .filter(|r| {
                let w: Vec<u16> = r.encode_utf16().chain(std::iter::once(0)).collect();
                // SAFETY: a NUL-terminated wide string.
                unsafe { GetDriveTypeW(w.as_ptr()) == 3 }
            })
            .map(PathBuf::from)
            .collect()
    }
    #[cfg(not(windows))]
    {
        vec![PathBuf::from("/")]
    }
}

pub fn home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from)
}

/// The name shown as the console player: trimmed, printable, at most 15 characters (the
/// console's gamertag length); empty means the engine's default.
pub fn clean_name(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect::<String>().trim().chars().take(15).collect()
}

/// The environment switch that carries the player name to the runtime.
pub const NAME_SWITCH: &str = "SCARLETT_GAMERTAG";

/// What the shader cache was translated with: the engine's translator fingerprint when it
/// reports one (`scarlett fingerprint`), else the engine build itself (older engines).
pub fn shader_key(translator: Option<&str>, engine_hash: Option<&str>) -> String {
    match (translator.map(str::trim).filter(|t| !t.is_empty()), engine_hash.map(str::trim).filter(|h| !h.is_empty())) {
        (Some(t), _) => format!("translator:{t}"),
        (None, Some(h)) => format!("build:{h}"),
        (None, None) => String::new(),
    }
}

/// `translator <hex>` in the output of `scarlett fingerprint`.
pub fn parse_fingerprint(out: &str) -> Option<String> {
    out.lines().find_map(|l| l.trim().strip_prefix("translator").map(str::trim).filter(|h| h.len() >= 16 && h.bytes().all(|b| b.is_ascii_hexdigit())).map(str::to_ascii_lowercase))
}

/// Asks an engine for its translator fingerprint (None for engines without the command).
pub fn fingerprint(exe: &Path) -> Option<String> {
    crate::cli::run(exe, &["fingerprint".into()], &[]).ok().and_then(|o| parse_fingerprint(&o))
}

/// The D3D12 Agility folder the runtime loads from beside its exe. When `exe_dir` lacks it and
/// `source` (the bundled runtime folder) has it, it is copied over. Returns what was done.
pub fn ensure_d3d12(exe_dir: &Path, source: &Path) -> Result<Option<String>, String> {
    let want = exe_dir.join("D3D12");
    if want.join("D3D12Core.dll").is_file() {
        return Ok(None);
    }
    let from = source.join("D3D12");
    if !from.join("D3D12Core.dll").is_file() || same(exe_dir, source) {
        return Err(format!("No D3D12\\D3D12Core.dll beside {}: extract the whole release ZIP (runtime\\D3D12).", exe_dir.display()));
    }
    std::fs::create_dir_all(&want).map_err(|e| format!("could not create {}: {e}", want.display()))?;
    for e in std::fs::read_dir(&from).map_err(|e| e.to_string())?.flatten() {
        if e.path().is_file() {
            std::fs::copy(e.path(), want.join(e.file_name())).map_err(|x| format!("could not copy {}: {x}", e.path().display()))?;
        }
    }
    Ok(Some(format!("Copied the D3D12 folder to {}", want.display())))
}

fn same(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

/// Suspends or resumes every thread of a running process (Pause in the preparation).
#[cfg(windows)]
pub fn set_suspended(child: &std::process::Child, suspend: bool) -> bool {
    use std::os::windows::io::AsRawHandle;
    #[link(name = "ntdll")]
    extern "system" {
        fn NtSuspendProcess(h: *mut core::ffi::c_void) -> i32;
        fn NtResumeProcess(h: *mut core::ffi::c_void) -> i32;
    }
    let h = child.as_raw_handle() as *mut core::ffi::c_void;
    // SAFETY: the handle belongs to a child we own and have not waited on.
    (unsafe { if suspend { NtSuspendProcess(h) } else { NtResumeProcess(h) } }) >= 0
}

/// The affinity mask that leaves the first `free` of `cores` logical cores to the desktop;
/// None when the PC has fewer than 6 (the game then keeps them all).
pub fn spare_mask(cores: usize, free: usize) -> Option<usize> {
    let cores = cores.min(usize::BITS as usize);
    if cores < 6 || free >= cores {
        return None;
    }
    let all = if cores == usize::BITS as usize { usize::MAX } else { (1usize << cores) - 1 };
    Some(all & !((1usize << free) - 1))
}

/// Keeps a running child off the first two logical cores (see [`spare_mask`]).
#[cfg(windows)]
pub fn leave_cores(child: &std::process::Child, cores: usize) -> bool {
    use std::os::windows::io::AsRawHandle;
    #[link(name = "kernel32")]
    extern "system" {
        fn SetProcessAffinityMask(h: *mut core::ffi::c_void, mask: usize) -> i32;
    }
    let Some(mask) = spare_mask(cores, 2) else { return false };
    // SAFETY: the handle belongs to a child we own and have not waited on.
    unsafe { SetProcessAffinityMask(child.as_raw_handle() as *mut core::ffi::c_void, mask) != 0 }
}

#[cfg(not(windows))]
pub fn leave_cores(_: &std::process::Child, _: usize) -> bool {
    false
}

#[cfg(not(windows))]
pub fn set_suspended(_: &std::process::Child, _: bool) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spare_mask_leaves_two_cores() {
        assert_eq!(spare_mask(4, 2), None);
        assert_eq!(spare_mask(6, 2), Some(0b111100));
        assert_eq!(spare_mask(16, 2), Some(0xFFFC));
        assert_eq!(spare_mask(64, 2), Some(usize::MAX & !3));
    }

    fn tmp(n: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sp-fr-{n}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn finds_games_within_depth_and_skips_system_folders() {
        let d = tmp("find");
        let exe = "game_scarlett_master.exe";
        for p in ["Games/A/Content", "Other/B", "Windows/C", "deep/1/2/3/D"] {
            std::fs::create_dir_all(d.join(p)).unwrap();
            std::fs::write(d.join(p).join(exe), "x").unwrap();
        }
        let cancel = AtomicBool::new(false);
        let mut got = Vec::new();
        let done = search(&starts(&[d.clone()], None), exe, Instant::now() + Duration::from_secs(10), &cancel, |p| got.push(p));
        assert!(done);
        let names: Vec<String> = got.iter().map(|p| p.strip_prefix(&d).unwrap().to_string_lossy().replace('\\', "/")).collect();
        assert!(names.contains(&"Games/A/Content".to_string()), "{names:?}");
        assert!(names.contains(&"Other/B".to_string()));
        assert!(!names.iter().any(|n| n.starts_with("Windows")), "system folders are skipped");
        assert!(!names.iter().any(|n| n.starts_with("deep")), "too deep under a plain root");
        assert_eq!(names.len(), 2, "each folder once: {names:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn search_stops_on_cancel_and_deadline() {
        let d = tmp("cancel");
        let cancel = AtomicBool::new(true);
        assert!(!search(&[(d.clone(), 2)], "x.exe", Instant::now() + Duration::from_secs(5), &cancel, |_| {}));
        let cancel = AtomicBool::new(false);
        assert!(!search(&[(d.clone(), 2)], "x.exe", Instant::now() - Duration::from_secs(1), &cancel, |_| {}));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn names_and_keys() {
        assert_eq!(clean_name("  Ziggy\n "), "Ziggy");
        assert_eq!(clean_name("abcdefghijklmnopqrst"), "abcdefghijklmno");
        assert_eq!(clean_name(""), "");
        assert_eq!(shader_key(Some("abc"), Some("h")), "translator:abc");
        assert_eq!(shader_key(None, Some("h")), "build:h");
        assert_eq!(shader_key(Some(" "), None), "");
        let fp = "0123456789abcdef0123456789abcdef";
        assert_eq!(parse_fingerprint(&format!("translator {fp}\n")), Some(fp.into()));
        assert_eq!(parse_fingerprint("error: unrecognized subcommand"), None);
        assert_eq!(parse_fingerprint("translator xyz"), None);
    }

    #[test]
    fn d3d12_is_copied_beside_a_build_that_lacks_it() {
        let src = tmp("d3d-src");
        let dst = tmp("d3d-dst");
        std::fs::create_dir_all(src.join("D3D12")).unwrap();
        std::fs::write(src.join("D3D12").join("D3D12Core.dll"), "core").unwrap();
        assert!(ensure_d3d12(&dst, &src).unwrap().is_some());
        assert_eq!(std::fs::read_to_string(dst.join("D3D12").join("D3D12Core.dll")).unwrap(), "core");
        assert_eq!(ensure_d3d12(&dst, &src).unwrap(), None, "already there");
        let empty = tmp("d3d-none");
        assert!(ensure_d3d12(&empty, &empty).is_err());
        for d in [src, dst, empty] {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}
