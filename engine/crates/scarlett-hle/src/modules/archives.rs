//! Mods that replace files inside the title's archives (modloader phase 2), and the archive
//! tables as the title reads them (`SCARLETT_RPF_TABLES=1`, for the format work).
//!
//! The World engine reads an archive's header and table (RPF9: magic `9FPR`), decrypts the table
//! in place, then parses it: the call to the parse at image offset `0xfc267b`, to `0x87aec0`, in
//! the archive object's constructor. That call goes through [`parse`] here. After the title's
//! parse the table is plain in memory, and:
//!
//! * **Entries mods replace** are pointed past the end of the archive file. A mod ships a file at
//!   its path inside the archive (`mods/<mod>/common.rpf/data/version.txt`); its entry, found by
//!   the name hash, gets an offset in a range after the file's real end and the mod file's size,
//!   and is marked stored as it is (no compression, no digests). The title's reads in that range,
//!   through `ReadFile` ([`serve_read`]) or DirectStorage ([`Tail::read`]), are served from the
//!   mod files, and `GetFileSizeEx` reports the size with the range ([`virtual_size`]).
//! * **The tables** are written to `<data_dir>/rpf-tables/` when asked for. They hold the title's
//!   tables (names and places of its files), so they stay in the data folder, outside the
//!   repository.
//!
//! In another build of the image the call is somewhere else and nothing is changed
//! (`redirect_call` checks it): mods inside archives are then not applied, and the log says so.
//!
//! The table (see `docs/modloader-phase2-notes.md`): a 64-byte header, then sections, a directory
//! of columns (24 bytes each: tag, rows, offset, row size, flags), a map giving each entry's row
//! in each column (all ones: none), and the columns' data. The parse keeps pointers to the
//! directory, the map and the data at `+0x20`, `+0x28` and `+0x30` of its state.

use std::collections::{HashMap, HashSet};
use std::os::windows::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

/// The call to the table parse in the archive constructor, and the parse, as image offsets.
const PARSE_CALL: usize = 0xfc_267b;
const PARSE: usize = 0x87_aec0;
const MAGIC: u32 = 0x5250_4639;
/// No archive table read so far is near this; past it the header is not believed.
const MOST: usize = 64 << 20;

/// Column tags.
const MAIN: u64 = 0x33f7_0dfa_f2c5_0989;
const NAME: u64 = 0x73bc_b72d_ce74_f11f;
const COMPRESSION: u64 = 0xe0c1_2d7a_5d48_84c5;
const DIGESTS: u64 = 0x2860_30a3_58a7_89fb;

/// Mod files start this far apart in the range after an archive's end.
const ALIGN: u64 = 4096;

type Parse = unsafe extern "system" fn(*mut u8, u32, *const u8, *const u8) -> usize;

static ORIGINAL: AtomicUsize = AtomicUsize::new(0);

// The name hash.

/// MurmurHash3 x64_128 with seed 0, the low 64 bits: the title's hash of paths in an archive.
fn murmur3(data: &[u8]) -> u64 {
    const C1: u64 = 0x87c3_7b91_1142_53d5;
    const C2: u64 = 0x4cf5_ad43_2745_937f;
    fn fmix(mut k: u64) -> u64 {
        k ^= k >> 33;
        k = k.wrapping_mul(0xff51_afd7_ed55_8ccd);
        k ^= k >> 33;
        k = k.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
        k ^ (k >> 33)
    }
    let word = |b: &[u8]| {
        let mut w = [0u8; 8];
        w[..b.len()].copy_from_slice(b);
        u64::from_le_bytes(w)
    };
    let (mut h1, mut h2) = (0u64, 0u64);
    let blocks = data.chunks_exact(16);
    let tail = blocks.remainder();
    for block in blocks {
        let k1 = word(&block[..8])
            .wrapping_mul(C1)
            .rotate_left(31)
            .wrapping_mul(C2);
        h1 ^= k1;
        h1 = h1
            .rotate_left(27)
            .wrapping_add(h2)
            .wrapping_mul(5)
            .wrapping_add(0x52dc_e729);
        let k2 = word(&block[8..])
            .wrapping_mul(C2)
            .rotate_left(33)
            .wrapping_mul(C1);
        h2 ^= k2;
        h2 = h2
            .rotate_left(31)
            .wrapping_add(h1)
            .wrapping_mul(5)
            .wrapping_add(0x3849_5ab5);
    }
    if tail.len() > 8 {
        h2 ^= word(&tail[8..])
            .wrapping_mul(C2)
            .rotate_left(33)
            .wrapping_mul(C1);
    }
    if !tail.is_empty() {
        h1 ^= word(&tail[..tail.len().min(8)])
            .wrapping_mul(C1)
            .rotate_left(31)
            .wrapping_mul(C2);
    }
    let n = data.len() as u64;
    h1 ^= n;
    h2 ^= n;
    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);
    h1 = fmix(h1);
    h2 = fmix(h2);
    h1.wrapping_add(h2)
}

/// The title's hash of `path` in an archive: `A`-`Z` lower case, `\` as `/`.
fn name_hash(path: &str) -> u64 {
    let bytes: Vec<u8> = path
        .bytes()
        .map(|b| {
            if b == b'\\' {
                b'/'
            } else {
                b.to_ascii_lowercase()
            }
        })
        .collect();
    murmur3(&bytes)
}

// What mods put in archives.

/// A mod file standing in for an entry of an archive.
pub struct Part {
    /// Its path inside the archive, as the mod ships it (`/` separators).
    pub path: String,
    /// The mod it comes from.
    pub by: String,
    host: PathBuf,
    len: u64,
    /// Where it lies in the range after the archive's end.
    at: u64,
    /// The name hashes it may have: of the path, and of the path without its extension.
    hashes: [u64; 2],
    file: OnceLock<Option<std::fs::File>>,
    /// Whether an entry took it, and whether that none did was logged.
    applied: AtomicBool,
    reported: AtomicBool,
    /// Whether the title has read it yet (logged the first time).
    read: AtomicBool,
}

/// An archive mods change entries of, and the range after its end that their files lie in.
pub struct Tail {
    /// The archive's path on the drive, as the mods folder names it (`COMMON.RPF`).
    pub key: String,
    /// Its host file, as [`host_key`] gives it.
    host: String,
    /// The hash in its header (offset 0x10), which the table the title parses carries too.
    header_hash: u64,
    /// The real file's size, and the end of the range.
    real: u64,
    end: u64,
    parts: Vec<Part>,
    /// Whether the title has opened it, and read past its real end, yet (logged the first time).
    seen_open: AtomicBool,
    seen_beyond: AtomicBool,
    seen_size: AtomicBool,
}

impl Tail {
    /// Reads at `offset` in the range after the real end (zeros between the files), as
    /// [`FileExt::seek_read`] does: 0 at the end.
    pub fn read(&self, offset: u64, out: &mut [u8]) -> std::io::Result<usize> {
        if !self.seen_beyond.swap(true, Ordering::Relaxed) {
            tracing::info!(archive = %self.key, offset, len = out.len(), "title reads past the real end of an archive mods change");
        }
        if offset >= self.end || out.is_empty() {
            return Ok(0);
        }
        let n = (self.end - offset).min(out.len() as u64) as usize;
        let out = &mut out[..n];
        out.fill(0);
        let stop = offset + n as u64;
        for part in &self.parts {
            let (from, to) = (part.at.max(offset), (part.at + part.len).min(stop));
            if from >= to {
                continue;
            }
            let Some(file) = part
                .file
                .get_or_init(|| std::fs::File::open(&part.host).ok())
            else {
                return Err(std::io::Error::other(format!(
                    "the mod file {} could not be opened",
                    part.host.display()
                )));
            };
            if !part.read.swap(true, Ordering::Relaxed) {
                tracing::info!(archive = %self.key, path = %part.path, r#mod = %part.by, "title reads a mod's file inside an archive");
            }
            let dest = &mut out[(from - offset) as usize..(to - offset) as usize];
            let mut done = 0;
            while done < dest.len() {
                let got = file.seek_read(&mut dest[done..], from - part.at + done as u64)?;
                if got == 0 {
                    break;
                }
                done += got;
            }
        }
        Ok(n)
    }

    /// The real file's size.
    pub fn real(&self) -> u64 {
        self.real
    }
}

/// The archives mods change, by the hash in their header and by host file.
#[derive(Default)]
struct Tails {
    by_hash: HashMap<u64, Arc<Tail>>,
    by_host: HashMap<String, Arc<Tail>>,
}

static TAILS: RwLock<Option<Tails>> = RwLock::new(None);
/// Whether any archive has mods, read before the locks.
static ANY: AtomicBool = AtomicBool::new(false);

/// A host path for comparing: no `\\?\`, `\` separators, lower case.
fn host_key(path: &str) -> String {
    let path = path.strip_prefix(r"\\?\").unwrap_or(path);
    path.replace('/', "\\").to_lowercase()
}

/// What [`set`] made of the mods' files in archives.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Planned {
    /// Archives with entries to replace, and how many files each.
    pub archives: Vec<(String, usize)>,
    /// Archive paths the dump does not have as an archive (their files are not used).
    pub missing: Vec<String>,
}

/// Plans the mods' files in archives. `wanted` maps an archive's path on the drive
/// (`COMMON.RPF`, `\` separators) to the files for it, as (path inside the archive, mod, host
/// file), the winning mod's only. `file_for` gives the host file the title opens for an archive's
/// drive path: the dump's, or a mod's copy of the whole archive. Replaces an earlier plan.
pub fn set(
    file_for: impl Fn(&str) -> PathBuf,
    wanted: Vec<(String, Vec<(String, String, PathBuf)>)>,
) -> Planned {
    let mut tails = Tails::default();
    let mut planned = Planned::default();
    for (key, files) in wanted {
        let file = file_for(&key);
        let file = std::path::absolute(&file).unwrap_or(file);
        let mut head = [0u8; 0x18];
        let read = std::fs::File::open(&file).and_then(|mut f| {
            let len = f.metadata()?.len();
            std::io::Read::read_exact(&mut f, &mut head).map(|()| len)
        });
        let real = match read {
            Ok(real) if u32::from_le_bytes(head[..4].try_into().unwrap()) == MAGIC => real,
            _ => {
                planned.missing.push(key);
                continue;
            }
        };
        let mut at = real.next_multiple_of(ALIGN);
        let mut parts = Vec::with_capacity(files.len());
        for (path, by, host) in files {
            let Ok(len) = std::fs::metadata(&host).map(|m| m.len()) else {
                continue;
            };
            // The title's names sometimes leave the extension out (it compares a type instead).
            let bare = match path.rsplit_once('.') {
                Some((stem, ext)) if !stem.is_empty() && !ext.contains('/') => stem.to_owned(),
                _ => path.clone(),
            };
            parts.push(Part {
                hashes: [name_hash(&path), name_hash(&bare)],
                path,
                by,
                host,
                len,
                at,
                file: OnceLock::new(),
                applied: AtomicBool::new(false),
                reported: AtomicBool::new(false),
                read: AtomicBool::new(false),
            });
            at = (at + len.max(1)).next_multiple_of(ALIGN);
        }
        let end = parts
            .last()
            .map_or(real, |p| (p.at + p.len).next_multiple_of(16));
        planned.archives.push((key.clone(), parts.len()));
        let tail = Arc::new(Tail {
            key,
            host: host_key(&file.to_string_lossy()),
            header_hash: u64::from_le_bytes(head[0x10..0x18].try_into().unwrap()),
            real,
            end,
            parts,
            seen_open: AtomicBool::new(false),
            seen_beyond: AtomicBool::new(false),
            seen_size: AtomicBool::new(false),
        });
        tails.by_hash.insert(tail.header_hash, tail.clone());
        tails.by_host.insert(tail.host.clone(), tail);
    }
    let any = !tails.by_hash.is_empty();
    *TAILS.write().unwrap_or_else(|e| e.into_inner()) = Some(tails);
    HANDLES.write().unwrap_or_else(|e| e.into_inner()).clear();
    ANY.store(any, Ordering::SeqCst);
    planned
}

/// Whether mods change entries of any archive (the parse must then be hooked).
pub fn any() -> bool {
    ANY.load(Ordering::Relaxed)
}

fn tail_by_hash(hash: u64) -> Option<Arc<Tail>> {
    TAILS
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()?
        .by_hash
        .get(&hash)
        .cloned()
}

/// The archive mods change whose host file is `path` (as opened), if any.
pub(crate) fn tail_for_host(path: &str) -> Option<Arc<Tail>> {
    if !any() {
        return None;
    }
    let key = host_key(path);
    TAILS
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()?
        .by_host
        .get(&key)
        .cloned()
}

// The title's handles to those archives.

type Handle = *mut std::ffi::c_void;

/// A file's identity, volume and file index: a handle closed and its value used again for
/// another file is told apart by it.
fn identity(handle: Handle) -> Option<(u32, u64)> {
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    // SAFETY: plain data, filled in below.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: asks about a handle; changes nothing.
    if unsafe { GetFileInformationByHandle(handle, &mut info) } == 0 {
        return None;
    }
    let index = (info.nFileIndexHigh as u64) << 32 | info.nFileIndexLow as u64;
    Some((info.dwVolumeSerialNumber, index))
}

type Opened = (usize, Arc<Tail>, (u32, u64));

static HANDLES: RwLock<Vec<Opened>> = RwLock::new(Vec::new());

/// Notes a handle the title opened at host path `path` (UTF-16, NUL-terminated).
pub(crate) fn opened(handle: Handle, path: *const u16) {
    if !any() || handle.is_null() || handle as isize == -1 || path.is_null() {
        return;
    }
    // SAFETY: the host path just opened, NUL-terminated.
    let path = unsafe {
        let len = (0..).take_while(|&i| *path.add(i) != 0).count();
        String::from_utf16_lossy(std::slice::from_raw_parts(path, len))
    };
    let Some(tail) = tail_for_host(&path) else {
        return;
    };
    let Some(id) = identity(handle) else {
        return;
    };
    if !tail.seen_open.swap(true, Ordering::Relaxed) {
        tracing::info!(archive = %tail.key, %path, "title opens an archive mods change");
    }
    let mut handles = HANDLES.write().unwrap_or_else(|e| e.into_inner());
    handles.retain(|(h, _, _)| *h != handle as usize);
    handles.push((handle as usize, tail, id));
}

/// The archive `handle` is open on, if mods change it (and the handle is still that file's).
pub(crate) fn tail_of(handle: Handle) -> Option<Arc<Tail>> {
    if !any() {
        return None;
    }
    let (tail, id) = {
        let handles = HANDLES.read().unwrap_or_else(|e| e.into_inner());
        let (_, tail, id) = handles.iter().find(|(h, _, _)| *h == handle as usize)?;
        (tail.clone(), *id)
    };
    if identity(handle) != Some(id) {
        let mut handles = HANDLES.write().unwrap_or_else(|e| e.into_inner());
        handles.retain(|(h, _, _)| *h != handle as usize);
        return None;
    }
    Some(tail)
}

/// For the file at host path `path` (UTF-16, NUL-terminated): when mods change entries of that
/// archive, the size with the range after its end goes into `data` as the title's
/// `GetFileAttributesExW` and `FindFirstFileW` give it (high half at 28, low at 32). The title's
/// archive device takes an archive's size from there and turns away entries past it.
///
/// # Safety
/// `data` is a `WIN32_FILE_ATTRIBUTE_DATA` or `WIN32_FIND_DATAW` the host just filled.
pub(crate) unsafe fn sized(path: &str, data: *mut u8) {
    let Some(tail) = tail_for_host(path) else {
        return;
    };
    if !tail.seen_size.swap(true, Ordering::Relaxed) {
        tracing::info!(archive = %tail.key, real = tail.real, size = tail.end, "title asks an archive's size by path; told it with the mods' range");
    }
    // SAFETY: by the caller; both structures keep the size there.
    unsafe {
        data.add(28)
            .cast::<u32>()
            .write_unaligned((tail.end >> 32) as u32);
        data.add(32).cast::<u32>().write_unaligned(tail.end as u32);
    }
}

/// The size `GetFileSizeEx` reports for `handle`: with the range after the end, for an archive
/// mods change.
pub(crate) fn virtual_size(handle: Handle) -> Option<u64> {
    tail_of(handle).map(|t| t.end)
}

/// A title's `ReadFile` on an archive mods change that reaches past the real end: served here,
/// the real part through `host_read` and the rest from the mod files. `None` for every other
/// read, which the caller does itself.
///
/// # Safety
/// The arguments are the title's `ReadFile` arguments: `buffer` holds `len` bytes, `read` and
/// `overlapped` are null or valid.
pub(crate) unsafe fn serve_read(
    handle: Handle,
    buffer: *mut u8,
    len: u32,
    read: *mut u32,
    overlapped: *mut u8,
    host_read: impl Fn(Handle, *mut u8, u32, *mut u32, Handle) -> i32,
) -> Option<i32> {
    use windows_sys::Win32::Foundation::{ERROR_HANDLE_EOF, SetLastError};
    use windows_sys::Win32::Storage::FileSystem::{FILE_CURRENT, SetFilePointerEx};
    if !any() || len == 0 {
        return None;
    }
    let offset = if overlapped.is_null() {
        let mut at = 0i64;
        // SAFETY: asks the title's handle for its position.
        if unsafe { SetFilePointerEx(handle, 0, &mut at, FILE_CURRENT) } == 0 {
            return None;
        }
        at as u64
    } else {
        // SAFETY: OVERLAPPED's Offset and OffsetHigh, at 16.
        unsafe { overlapped.add(16).cast::<u64>().read_unaligned() }
    };
    let real = {
        let handles = HANDLES.read().unwrap_or_else(|e| e.into_inner());
        handles
            .iter()
            .find(|(h, _, _)| *h == handle as usize)?
            .1
            .real
    };
    if offset + len as u64 <= real {
        return None;
    }
    let tail = tail_of(handle)?;
    let mut done = 0usize;
    if offset < tail.real {
        // The real part, as the title asked for it but shorter.
        let first = (tail.real - offset) as u32;
        let mut got = 0u32;
        if host_read(handle, buffer, first, &mut got, overlapped.cast()) == 0 {
            return Some(0);
        }
        done = got as usize;
        if got < first {
            return Some(finish(handle, read, overlapped, done, offset));
        }
    }
    let mut bytes = vec![0u8; len as usize - done];
    let got = tail.read(offset + done as u64, &mut bytes).unwrap_or_else(|error| {
        tracing::warn!(archive = %tail.key, %error, "a mod file inside an archive could not be read");
        0
    });
    // Copied from user mode, so a guarded page of the title's faults into its handler.
    // SAFETY: `buffer` holds `len` bytes and `done + got <= len`.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer.add(done), got) };
    done += got;
    if done == 0 && !overlapped.is_null() {
        // SAFETY: plain call.
        unsafe { SetLastError(ERROR_HANDLE_EOF) };
        return Some(0);
    }
    Some(finish(handle, read, overlapped, done, offset))
}

/// A read served here finished as a synchronous `ReadFile` finishes: the count, the position,
/// the OVERLAPPED's status and event.
fn finish(handle: Handle, read: *mut u32, overlapped: *mut u8, done: usize, offset: u64) -> i32 {
    use windows_sys::Win32::Foundation::SetLastError;
    use windows_sys::Win32::Storage::FileSystem::{FILE_BEGIN, SetFilePointerEx};
    use windows_sys::Win32::System::Threading::SetEvent;
    // SAFETY: the title's handle, `read` and OVERLAPPED, as it passed them.
    unsafe {
        let to = (offset + done as u64) as i64;
        SetFilePointerEx(handle, to, std::ptr::null_mut(), FILE_BEGIN);
        if !read.is_null() {
            *read = done as u32;
        }
        if !overlapped.is_null() {
            overlapped.cast::<usize>().write_unaligned(0);
            overlapped.add(8).cast::<usize>().write_unaligned(done);
            let event = overlapped.add(24).cast::<usize>().read_unaligned() & !1;
            if event != 0 {
                SetEvent(event as Handle);
            }
        }
        SetLastError(0);
    }
    1
}

// The hook.

struct Out {
    dir: PathBuf,
    /// Header hash (offset 0x10) of each archive file in the title's folder, and its name.
    names: HashMap<u64, String>,
    seen: Mutex<HashSet<u64>>,
}

static OUT: OnceLock<Out> = OnceLock::new();

/// Whether `SCARLETT_RPF_TABLES=1` asks for the tables.
pub fn asked() -> bool {
    std::env::var_os("SCARLETT_RPF_TABLES").is_some_and(|v| v == "1")
}

/// The hash at offset 0x10 of the header of each `.rpf` in `title_dir`, by file name.
fn names(title_dir: &Path) -> HashMap<u64, String> {
    let mut names = HashMap::new();
    let Ok(dir) = std::fs::read_dir(title_dir) else {
        return names;
    };
    for entry in dir.flatten() {
        let path = entry.path();
        if !path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("rpf"))
        {
            continue;
        }
        let mut head = [0u8; 0x18];
        let read = std::fs::File::open(&path)
            .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut head));
        if read.is_ok() && u32::from_le_bytes(head[..4].try_into().unwrap()) == MAGIC {
            let hash = u64::from_le_bytes(head[0x10..0x18].try_into().unwrap());
            names.insert(hash, entry.file_name().to_string_lossy().into_owned());
        }
    }
    names
}

/// Sends the title's table parse through [`parse`]: for mods inside archives ([`any`]), and for
/// the tables when `tables` names a folder for them.
///
/// # Safety
/// The image is mapped at `base` with `size` bytes and none of its code has run yet.
pub unsafe fn install(base: usize, size: usize, title_dir: &Path, tables: Option<PathBuf>) {
    if let Some(dir) = &tables {
        let _ = OUT.set(Out {
            dir: dir.clone(),
            names: names(title_dir),
            seen: Mutex::new(HashSet::new()),
        });
    }
    let ours = parse as *const () as usize;
    // SAFETY: by the caller; `parse` takes what the title's parse takes.
    match unsafe { scarlett_loader::hooks::redirect_call(base, size, PARSE_CALL, PARSE, ours) } {
        Ok(original) => {
            ORIGINAL.store(original, Ordering::SeqCst);
            if let Some(dir) = &tables {
                tracing::info!(dir = %dir.display(), "archive tables are written as the title reads them (SCARLETT_RPF_TABLES); they hold game data and stay outside the repository");
            }
        }
        Err(why) => {
            if any() {
                tracing::warn!(
                    ?why,
                    "mods inside archives are not applied: this build's archive code is not where it is looked for"
                );
            }
            if tables.is_some() {
                tracing::warn!(
                    ?why,
                    "archive tables not written: this build's archive code is not where it is looked for (SCARLETT_RPF_TABLES)"
                );
            }
        }
    }
}

/// Bits per byte of `bytes`: near 8 for what is still encrypted or compressed.
fn entropy(bytes: &[u8]) -> f64 {
    let mut counts = [0usize; 256];
    for &b in bytes {
        counts[b as usize] += 1;
    }
    let n = bytes.len() as f64;
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| c as f64 / n)
        .map(|p| -p * p.log2())
        .sum()
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap())
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// The bytes the title read for the table: the whole when the mode is 0, else up to the trailer
/// (as `0x141fb2600` reads them); `None` for what is not an archive table.
///
/// # Safety
/// `blob` is what the title passed its parse.
unsafe fn table<'a>(blob: *const u8, mode: u32) -> Option<&'a mut [u8]> {
    if blob.is_null() {
        return None;
    }
    // SAFETY: the header's first 64 bytes, which the title read before it called the parse.
    let head = unsafe { std::slice::from_raw_parts(blob, 0x40) };
    if u32_at(head, 0) != MAGIC {
        return None;
    }
    let whole = u32_at(head, 4) as usize;
    let table = u32_at(head, 8) as usize;
    let len = if mode == 0 { whole } else { table };
    if len < 0x40 || len > MOST || table > whole {
        return None;
    }
    // SAFETY: the title read `len` bytes into the buffer it allocated for them, its own heap's.
    Some(unsafe { std::slice::from_raw_parts_mut(blob.cast_mut(), len) })
}

/// Where the parse found the parts of `table`: (directory, map, data), as offsets in it.
fn layout(table: &[u8], state: &[u8]) -> Option<(usize, usize, usize)> {
    let blob = table.as_ptr() as usize;
    let at = |i: usize| {
        (u64_at(state, i) as usize)
            .checked_sub(blob)
            .filter(|&o| o <= table.len())
    };
    let (dir, map, data) = (at(0x20)?, at(0x28)?, at(0x30)?);
    (u64_at(state, 8) as usize == blob && dir <= map && map <= data).then_some((dir, map, data))
}

/// A column of the table: its rows' offset in the table, row size, rows, and its slot in each
/// entry's map record (`None` for a trailer column).
struct Column {
    at: usize,
    size: usize,
    rows: usize,
    slot: Option<usize>,
}

/// Points the entries `tail`'s mod files replace at them; returns how many it changed.
fn replace(table: &mut [u8], (dir, map, data): (usize, usize, usize), tail: &Tail) -> usize {
    let odd = |why: &str| {
        tracing::warn!(archive = %tail.key, why, "an archive table not laid out as expected: mods inside it are not applied");
        0
    };
    let columns = u16_at(table, 0x20) as usize;
    let trailing = u16_at(table, 0x22) as usize;
    let entries = u32_at(table, 0x24) as usize;
    let width = table[0x2c] as usize;
    if !(1..=2).contains(&width) || trailing > columns || dir + columns * 0x18 != map {
        return odd("header");
    }
    let slots = columns - trailing;
    if map + entries * slots * width > data {
        return odd("map");
    }
    let mut by_tag = HashMap::new();
    let mut slot = 0;
    for i in 0..columns {
        let row = dir + i * 0x18;
        let in_trailer = u16_at(table, row + 18) & 1 != 0;
        let column = Column {
            at: data + u32_at(table, row + 12) as usize,
            size: u16_at(table, row + 16) as usize,
            rows: u32_at(table, row + 8) as usize,
            slot: (!in_trailer).then(|| {
                slot += 1;
                slot - 1
            }),
        };
        if !in_trailer && column.at + column.rows * column.size > table.len() {
            return odd("column");
        }
        by_tag.insert(u64_at(table, row), column);
    }
    let (Some(main), Some(name)) = (by_tag.get(&MAIN), by_tag.get(&NAME)) else {
        return odd("columns");
    };
    let (Some(main_slot), Some(name_slot)) = (main.slot, name.slot) else {
        return odd("columns");
    };
    if main.size != 0x18 || name.size != 8 {
        return odd("row sizes");
    }
    let none = if width == 1 { 0xff } else { 0xffff };
    let index = |table: &[u8], entry: usize, slot: usize| {
        let at = map + (entry * slots + slot) * width;
        let i = if width == 1 {
            table[at] as usize
        } else {
            u16_at(table, at) as usize
        };
        (i != none).then_some(i)
    };
    // Each file takes the entry with its full name, or, when the archive has none, the entry
    // named without the extension; never both (`a.ymt` and `a` may be different entries).
    let present: HashSet<u64> = (0..entries)
        .filter_map(|e| index(table, e, name_slot).filter(|&r| r < name.rows))
        .map(|r| u64_at(table, name.at + r * 8))
        .collect();
    let wanted: HashMap<u64, &Part> = tail
        .parts
        .iter()
        .filter_map(|p| {
            let [full, bare] = p.hashes;
            [full, bare]
                .into_iter()
                .find(|h| present.contains(h))
                .map(|h| (h, p))
        })
        .collect();
    let clear: Vec<usize> = [COMPRESSION, DIGESTS]
        .iter()
        .filter_map(|tag| by_tag.get(tag).and_then(|c| c.slot))
        .collect();
    let mut changed = 0;
    for entry in 0..entries {
        let Some(r) = index(table, entry, name_slot).filter(|&r| r < name.rows) else {
            continue;
        };
        let Some(part) = wanted.get(&u64_at(table, name.at + r * 8)) else {
            continue;
        };
        let Some(m) = index(table, entry, main_slot).filter(|&m| m < main.rows) else {
            continue;
        };
        let record = &mut table[main.at + m * 0x18..][..0x18];
        // Byte 5 is the entry's kind (0xff an ordinary file, 0 an archive inside the archive, else
        // a resource of that type: 0x0b is PSO metadata) and is kept. How the data is stored is
        // said by byte 6 and the columns: a compression row is zlib in 64 KiB chunks, byte 6 = 1
        // with none is one raw deflate stream, and neither is the data as it is.
        let kind = record[5];
        record[..5].copy_from_slice(&(part.at / 16).to_le_bytes()[..5]);
        record[6] = 0;
        record[8..16].copy_from_slice(&part.len.div_ceil(16).to_le_bytes());
        record[16..24].copy_from_slice(&part.len.to_le_bytes());
        // Stored as it is: no compression parameters, no digests.
        for &slot in &clear {
            let at = map + (entry * slots + slot) * width;
            table[at..at + width].fill(0xff);
        }
        changed += 1;
        if !part.applied.swap(true, Ordering::Relaxed) {
            tracing::info!(archive = %tail.key, path = %part.path, r#mod = %part.by, bytes = part.len, kind, "an archive entry is replaced by a mod's file");
        }
    }
    for part in &tail.parts {
        if !part.applied.load(Ordering::Relaxed) && !part.reported.swap(true, Ordering::Relaxed) {
            tracing::warn!(archive = %tail.key, path = %part.path, r#mod = %part.by, "a mod's file matches no entry of its archive and is not used (adding entries is not supported yet)");
        }
    }
    changed
}

/// Writes one archive's header, table and trailer, once per archive.
///
/// # Safety
/// `trailer` is what the title passed its parse beside `table`.
unsafe fn write(table: &[u8], state: &[u8], mode: u32, trailer: *const u8) {
    let Some(out) = OUT.get() else {
        return;
    };
    let hash = u64_at(table, 0x10);
    if !out
        .seen
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(hash)
    {
        return;
    }
    let name = out
        .names
        .get(&hash)
        .cloned()
        .unwrap_or_else(|| format!("{hash:016x}.rpf"));
    let wrote = std::fs::write(out.dir.join(format!("{name}.table")), table);
    let (whole, len) = (u32_at(table, 4) as usize, u32_at(table, 8) as usize);
    let mut trailer_len = 0;
    if mode == 1 && !trailer.is_null() && whole > len {
        trailer_len = whole - len;
        // SAFETY: the trailer the title read beside the table, `whole - len` bytes.
        let t = unsafe { std::slice::from_raw_parts(trailer, trailer_len) };
        let _ = std::fs::write(out.dir.join(format!("{name}.trailer")), t);
    }
    let state: String = state.iter().map(|b| format!("{b:02x}")).collect();
    tracing::info!(
        archive = %name,
        mode,
        entries = u32_at(table, 0x24),
        key = format_args!("{:08x}", u32_at(table, 0x28)),
        bytes = table.len(),
        trailer = trailer_len,
        table_entropy = format_args!("{:.2}", entropy(&table[0x40..])),
        %state,
        written = wrote.is_ok(),
        "archive table as the title read it"
    );
}

/// After the title's parse: entries mods replace changed, then the table written if asked.
///
/// # Safety
/// What the title gave its parse.
unsafe fn after(state: *mut u8, mode: u32, blob: *const u8, trailer: *const u8) {
    // SAFETY: by the caller.
    let Some(table) = (unsafe { table(blob, mode) }) else {
        return;
    };
    // SAFETY: the parse's state, 0x38 bytes (`0x14087af70` writes up to 0x38).
    let state = unsafe { std::slice::from_raw_parts(state.cast_const(), 0x38) };
    if let Some(tail) = tail_by_hash(u64_at(table, 0x10)) {
        match layout(table, state) {
            Some(parts) => drop(replace(table, parts, &tail)),
            None => {
                tracing::warn!(archive = %tail.key, "an archive table parsed in a way not known: mods inside it are not applied")
            }
        }
    }
    // SAFETY: by the caller.
    unsafe { write(table, state, mode, trailer) };
}

/// The title's table parse, then [`after`].
unsafe extern "system" fn parse(
    state: *mut u8,
    mode: u32,
    blob: *const u8,
    trailer: *const u8,
) -> usize {
    // SAFETY: the address `redirect_call` returned, the title's parse, called as the title calls
    // it.
    let theirs: Parse = unsafe { std::mem::transmute(ORIGINAL.load(Ordering::SeqCst)) };
    let ret = unsafe { theirs(state, mode, blob, trailer) };
    // Nothing of ours may unwind into the title's code.
    // SAFETY: what the title gave its parse, unchanged.
    let _ = std::panic::catch_unwind(|| unsafe { after(state, mode, blob, trailer) });
    ret
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entropy_tells_plain_from_random() {
        assert!(entropy(&[0u8; 256]) < 0.01);
        let all: Vec<u8> = (0..=255).collect();
        assert!((entropy(&all) - 8.0).abs() < 1e-9);
    }

    #[test]
    fn names_come_from_the_header_hash() {
        let dir = tempfile::tempdir().unwrap();
        let mut head = vec![0u8; 0x40];
        head[..4].copy_from_slice(&MAGIC.to_le_bytes());
        head[0x10..0x18].copy_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
        std::fs::write(dir.path().join("Common.rpf"), &head).unwrap();
        std::fs::write(dir.path().join("short.rpf"), b"9FPR").unwrap();
        std::fs::write(dir.path().join("other.bin"), &head).unwrap();
        let names = names(dir.path());
        assert_eq!(names.len(), 1);
        assert_eq!(names[&0x1122_3344_5566_7788], "Common.rpf");
    }

    /// The published MurmurHash3 x64_128 values (low 64 bits), and the title's normalising.
    #[test]
    fn the_name_hash_is_murmur3() {
        assert_eq!(murmur3(b""), 0);
        assert_eq!(murmur3(b"hello"), 0xcbd8_a7b3_41bd_9b02);
        assert_eq!(name_hash(r"Data\Version.TXT"), murmur3(b"data/version.txt"));
    }

    /// A table made for the test: header, sections left zero, a directory of a main, a name and
    /// a compression column and a trailer column, the map, the data.
    fn synthetic(names: &[u64]) -> (Vec<u8>, (usize, usize, usize)) {
        let entries = names.len();
        let (dir, columns, trailing) = (0x1b0usize, 4usize, 1usize);
        let map = dir + columns * 0x18;
        let data = map + (entries * (columns - trailing)).next_multiple_of(8);
        let name_at = entries * 0x18;
        let comp_at = name_at + entries * 8;
        let mut t = vec![0u8; data + comp_at + 8];
        t[..4].copy_from_slice(&MAGIC.to_le_bytes());
        t[0x20..0x22].copy_from_slice(&(columns as u16).to_le_bytes());
        t[0x22..0x24].copy_from_slice(&(trailing as u16).to_le_bytes());
        t[0x24..0x28].copy_from_slice(&(entries as u32).to_le_bytes());
        t[0x2c] = 1;
        let rows = [
            (MAIN, entries, 0, 0x18u16, 0u16),
            (NAME, entries, name_at, 8, 0),
            (COMPRESSION, 1, comp_at, 8, 0),
            (0xb6f5_bdb2_c66d_a2e9, entries, 0, 8, 1),
        ];
        for (i, (tag, n, off, size, flags)) in rows.into_iter().enumerate() {
            let r = dir + i * 0x18;
            t[r..r + 8].copy_from_slice(&tag.to_le_bytes());
            t[r + 8..r + 12].copy_from_slice(&(n as u32).to_le_bytes());
            t[r + 12..r + 16].copy_from_slice(&(off as u32).to_le_bytes());
            t[r + 16..r + 18].copy_from_slice(&size.to_le_bytes());
            t[r + 18..r + 20].copy_from_slice(&flags.to_le_bytes());
        }
        for (e, &h) in names.iter().enumerate() {
            // Rows in reverse, so the map matters; only the last entry is compressed.
            let r = entries - 1 - e;
            t[map + e * 3] = r as u8;
            t[map + e * 3 + 1] = r as u8;
            t[map + e * 3 + 2] = if e == entries - 1 { 0 } else { 0xff };
            let rec = data + r * 0x18;
            t[rec..rec + 8].copy_from_slice(&(0x100u64 | 0xff << 40).to_le_bytes());
            t[rec + 8..rec + 16].copy_from_slice(&7u64.to_le_bytes());
            t[rec + 16..rec + 24].copy_from_slice(&200u64.to_le_bytes());
            t[data + name_at + r * 8..][..8].copy_from_slice(&h.to_le_bytes());
        }
        (t, (dir, map, data))
    }

    fn part(path: &str, at: u64, len: u64) -> Part {
        let bare = path.rsplit_once('.').map_or(path, |p| p.0);
        Part {
            path: path.into(),
            by: "m".into(),
            host: PathBuf::new(),
            len,
            at,
            hashes: [name_hash(path), name_hash(bare)],
            file: OnceLock::new(),
            applied: AtomicBool::new(false),
            reported: AtomicBool::new(false),
            read: AtomicBool::new(false),
        }
    }

    fn tail(real: u64, end: u64, parts: Vec<Part>) -> Tail {
        Tail {
            key: "X.RPF".into(),
            host: String::new(),
            header_hash: 0,
            real,
            end,
            parts,
            seen_open: AtomicBool::new(false),
            seen_beyond: AtomicBool::new(false),
            seen_size: AtomicBool::new(false),
        }
    }

    #[test]
    fn a_replaced_entry_points_past_the_end_stored_as_it_is() {
        let names = [name_hash("a.txt"), name_hash("data/b.meta"), name_hash("c")];
        let (mut t, layout) = synthetic(&names);
        let before = t.clone();
        let parts = vec![
            part("Data/B.meta", 0x2000, 33),
            part("c.dds", 0x3000, 1),
            part("none", 0, 1),
        ];
        let tail = tail(5000, 0x3010, parts);
        assert_eq!(replace(&mut t, layout, &tail), 2);
        let (_, map, data) = layout;
        // Entry 1 (row 1): offset 0x2000 / 16, stored as it is, 3 units, 33 bytes.
        let rec = &t[data + 0x18..][..0x18];
        assert_eq!(&rec[..6], &[0x00, 0x02, 0, 0, 0, 0xff]);
        assert_eq!(u64_at(rec, 8), 3);
        assert_eq!(u64_at(rec, 16), 33);
        // Entry 2 (row 0), named without its extension, lost its compression column.
        assert_eq!(u64_at(&t[data..], 16), 1);
        assert_eq!(t[map + 2 * 3 + 2], 0xff);
        // Entry 0 (row 2) is as it was.
        assert_eq!(
            &t[data + 2 * 0x18..][..0x18],
            &before[data + 2 * 0x18..][..0x18]
        );
        assert!(tail.parts[0].applied.load(Ordering::Relaxed));
        assert!(!tail.parts[2].applied.load(Ordering::Relaxed));
    }

    #[test]
    fn a_resource_entry_keeps_its_kind_and_is_stored_as_it_is() {
        let (mut t, layout) = synthetic(&[name_hash("data/a")]);
        let (_, _, data) = layout;
        // A PSO resource stored as one raw deflate stream (byte 6 = 1).
        t[data + 5] = 0x0b;
        t[data + 6] = 1;
        t[data + 7] = 0x22;
        let tail = tail(0, 0x2000, vec![part("data/a.pso", 0x1000, 5)]);
        assert_eq!(replace(&mut t, layout, &tail), 1);
        assert_eq!(&t[data..data + 8], &[0x00, 0x01, 0, 0, 0, 0x0b, 0, 0x22]);
    }

    #[test]
    fn the_full_name_wins_over_the_bare_one() {
        let names = [name_hash("x/a.ymt"), name_hash("x/a")];
        let (mut t, layout) = synthetic(&names);
        let tail = tail(0, 0x2000, vec![part("x/a.ymt", 0x1000, 5)]);
        assert_eq!(replace(&mut t, layout, &tail), 1);
        let (_, _, data) = layout;
        // Entry 0 (row 1) is the full name and changed; entry 1 (row 0) is as it was.
        assert_eq!(u64_at(&t[data + 0x18..], 16), 5);
        assert_eq!(u64_at(&t[data..], 16), 200);
    }

    #[test]
    fn a_table_laid_out_otherwise_is_left_alone() {
        let (mut t, (dir, map, data)) = synthetic(&[name_hash("a")]);
        let before = t.clone();
        let tail = tail(0, 0, vec![part("a", 0x1000, 1)]);
        assert_eq!(replace(&mut t, (dir, map + 8, data), &tail), 0);
        t[0x2c] = 3;
        assert_eq!(replace(&mut t, (dir, map, data), &tail), 0);
        t[0x2c] = 1;
        assert_eq!(&t[..], &before[..]);
    }

    #[test]
    fn the_range_reads_mod_files_and_zeros_between() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::write(&a, b"hello").unwrap();
        std::fs::write(&b, b"world!").unwrap();
        let mut pa = part("a", 0x1000, 5);
        pa.host = a;
        let mut pb = part("b", 0x2000, 6);
        pb.host = b;
        let tail = tail(100, 0x2010, vec![pa, pb]);
        let mut out = [7u8; 8];
        assert_eq!(tail.read(0x1003, &mut out).unwrap(), 8);
        assert_eq!(&out, b"lo\0\0\0\0\0\0");
        let mut out = [7u8; 32];
        assert_eq!(tail.read(0x2000, &mut out).unwrap(), 16);
        assert_eq!(&out[..16], b"world!\0\0\0\0\0\0\0\0\0\0");
        assert_eq!(tail.read(0x2010, &mut out).unwrap(), 0);
    }

    /// One test for the global plan, so no other test sees it half set.
    #[test]
    fn set_plans_a_range_after_each_archive_and_serves_reads_in_it() {
        use std::os::windows::io::AsRawHandle;
        let dump = tempfile::tempdir().unwrap();
        let mods = tempfile::tempdir().unwrap();
        let mut head = vec![0x55u8; 5000];
        head[..4].copy_from_slice(&MAGIC.to_le_bytes());
        head[0x10..0x18].copy_from_slice(&42u64.to_le_bytes());
        std::fs::write(dump.path().join("common.rpf"), &head).unwrap();
        let (f1, f2) = (mods.path().join("f1"), mods.path().join("f2"));
        std::fs::write(&f1, vec![1u8; 10]).unwrap();
        std::fs::write(&f2, vec![2u8; 5000]).unwrap();
        let files = vec![
            ("a.txt".into(), "m".into(), f1),
            ("b".into(), "m".into(), f2),
        ];
        let gone = vec![("x".into(), "m".into(), PathBuf::from("nowhere"))];
        let root = dump.path().to_path_buf();
        let planned = set(
            |key| root.join(key),
            vec![("COMMON.RPF".into(), files), ("GONE.RPF".into(), gone)],
        );
        assert_eq!(planned.archives, [("COMMON.RPF".to_owned(), 2)]);
        assert_eq!(planned.missing, ["GONE.RPF"]);
        let tail = tail_by_hash(42).unwrap();
        assert_eq!(
            (tail.real, tail.parts[0].at, tail.parts[1].at),
            (5000, 0x2000, 0x3000)
        );
        assert_eq!(tail.end, 0x3000 + 5000u64.next_multiple_of(16));

        // The title opens the archive and reads across its end.
        let path = std::path::absolute(dump.path().join("common.rpf")).unwrap();
        let wide: Vec<u16> = format!(r"\\?\{}", path.to_string_lossy().to_uppercase())
            .encode_utf16()
            .chain([0])
            .collect();
        let file = std::fs::File::open(&path).unwrap();
        let handle = file.as_raw_handle();
        opened(handle, wide.as_ptr());
        assert_eq!(virtual_size(handle), Some(tail.end));
        let host = |h: Handle, b: *mut u8, n: u32, r: *mut u32, o: Handle| {
            super::super::files::ReadFile(h, b, n, r, o)
        };
        let mut buf = vec![0u8; 0x2005 - 4990];
        let mut ov = [0u8; 32];
        ov[16..24].copy_from_slice(&4990u64.to_le_bytes());
        let mut got = 0u32;
        let ok = unsafe {
            serve_read(
                handle,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut got,
                ov.as_mut_ptr(),
                host,
            )
        };
        assert_eq!((ok, got as usize), (Some(1), buf.len()));
        assert!(buf[..10].iter().all(|&b| b == 0x55));
        assert!(buf[10..0x2000 - 4990].iter().all(|&b| b == 0));
        assert_eq!(&buf[0x2000 - 4990..], &[1u8; 5]);
        // Reads inside the real file are the caller's.
        ov[16..24].copy_from_slice(&0u64.to_le_bytes());
        let ok = unsafe {
            serve_read(
                handle,
                buf.as_mut_ptr(),
                16,
                &mut got,
                ov.as_mut_ptr(),
                host,
            )
        };
        assert_eq!(ok, None);

        set(|key| root.join(key), Vec::new());
        assert!(!any());
        assert_eq!(virtual_size(handle), None);
    }
}
