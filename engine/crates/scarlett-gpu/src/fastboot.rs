//! `SCARLETT_FAST_BOOT=1` (testing only, off by default): press through the
//! title's logos, menus and the account prompt ("ACCOUNT PICKER (Y) /
//! PLAYER (A)": A takes the signed-in player) without a person, then stop
//! for good once the title is in the world.
//!
//! "In the world" is read from the render targets the title binds per
//! presented frame: menus and loading bind a handful, the world a hundred or
//! more. [`THRESHOLD`] binds on [`SUSTAIN`] consecutive frames ends the mode.
//! `SCARLETT_FAST_BOOT_SUSTAIN=<frames>` asks for more world frames (a slow
//! loading screen can bind as many for a few frames, 5 s each), and
//! `SCARLETT_FAST_BOOT_GAP=<frames>` lets fewer than that many light frames
//! in a row come between them (default 1: any light frame starts again).
//! Every 30 frames an info line says what the detector sees.
//! The fallback is a time limit, `SCARLETT_FAST_BOOT_MAX_S` (default
//! [`MAX_SECONDS`]; `SCARLETT_FAST_BOOT_MAX_FRAMES` sets a frame limit too).
//! After the world is detected, with `SCARLETT_SKIP_CUTSCENES=1`, A is still
//! pressed for [`SKIP_SECONDS`] so the intro cutscene's prompt skips it.
//! `SCARLETT_FAST_BOOT_ESC=0` leaves out the START (Esc) presses: in the
//! world they open the pause menu, whose frames bind few targets, so the
//! world may never be seen.
//! Only our synthetic keys depend on this; the title's clocks and our frame
//! pacing are never touched.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

/// Render-target binds in one frame that mark a world frame.
pub const THRESHOLD: u32 = 100;
/// Consecutive world frames needed, unless `SCARLETT_FAST_BOOT_SUSTAIN` says.
pub const SUSTAIN: u32 = 3;
/// Fallback: fast boot ends this long after the start at the latest.
pub const MAX_SECONDS: u64 = 420;
/// After the world is detected: seconds A is still pressed (cutscene skip).
pub const SKIP_SECONDS: u64 = 25;

static WORLD_AT: OnceLock<Instant> = OnceLock::new();
static MAX_S: OnceLock<u64> = OnceLock::new();
static ENABLED: OnceLock<bool> = OnceLock::new();
static START: OnceLock<Instant> = OnceLock::new();
static IN_WORLD: AtomicBool = AtomicBool::new(false);
static BINDS: AtomicU32 = AtomicU32::new(0);
static FRAME: AtomicU64 = AtomicU64::new(0);
static MAX: OnceLock<u64> = OnceLock::new();
static RUN: AtomicU32 = AtomicU32::new(0);
static SUSTAIN_FRAMES: OnceLock<u32> = OnceLock::new();
static GAP_FRAMES: OnceLock<u32> = OnceLock::new();
/// Light frames in a row since the last world frame.
static LIGHT: AtomicU32 = AtomicU32::new(0);
/// World frames since the last "still booting" line.
static HEAVY: AtomicU32 = AtomicU32::new(0);

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

fn truthy(v: Option<String>) -> bool {
    v.is_some_and(|v| !matches!(v.trim(), "" | "0" | "false" | "off"))
}

/// Fast boot was asked for.
pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| {
        let on = truthy(env("SCARLETT_FAST_BOOT"));
        if on {
            START.get_or_init(Instant::now);
        }
        on
    })
}

/// Fast boot is on and the title is not in the world yet: synthetic input applies.
pub fn active() -> bool {
    enabled() && !IN_WORLD.load(Ordering::Relaxed)
}

/// `SCARLETT_SKIP_CUTSCENES=1` and the world was detected less than
/// [`SKIP_SECONDS`] ago: A is still pressed so the intro cutscene skips.
pub fn skipping() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| truthy(env("SCARLETT_SKIP_CUTSCENES")))
        && WORLD_AT.get().is_some_and(|t| t.elapsed().as_secs() < SKIP_SECONDS)
}

/// The detector saw the world (or gave up waiting). Without fast boot this
/// is always false.
pub fn in_world() -> bool {
    IN_WORLD.load(Ordering::Relaxed)
}

/// When fast boot ended (the world seen, or a limit reached), if it has.
/// `SCARLETT_KEYS_FROM_WORLD=1` times its keys from here.
pub fn world_at() -> Option<Instant> {
    WORLD_AT.get().copied()
}

/// START (Esc) is pressed while booting, as well as A: on unless
/// `SCARLETT_FAST_BOOT_ESC=0`.
pub fn esc() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| env("SCARLETT_FAST_BOOT_ESC").is_none_or(|v| truthy(Some(v))))
}

/// Presented frames seen so far (while fast boot is on).
pub fn frame() -> u64 {
    FRAME.load(Ordering::Relaxed)
}

/// A frame bound render targets (called per bind).
pub fn note_render_targets() {
    if enabled() || unpaced_loading() {
        BINDS.fetch_add(1, Ordering::Relaxed);
    }
}

/// `SCARLETT_LOAD_UNPACED=1` (or `SCARLETT_LOAD_UNPACED`; off by default):
/// while the title draws loading screens, logos and menus (frames that bind
/// fewer than [`THRESHOLD`] render targets), its frame waits end at once, so
/// those frames are not held to the title's 30 fps interval. World frames
/// are paced as before.
pub fn unpaced_loading() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = truthy(env("SCARLETT_LOAD_UNPACED").or_else(|| env("SCARLETT_LOAD_UNPACED")));
        if on {
            tracing::info!("load unpaced: frame waits end at once on loading and menu frames");
        }
        on
    })
}

/// Binds of the last presented frame (only kept with fast boot or
/// unpaced loading).
static LAST_BINDS: AtomicU32 = AtomicU32::new(0);

/// Unpaced loading is on and the last frame was a light one (not a world frame).
pub fn skip_pacing() -> bool {
    unpaced_loading() && LAST_BINDS.load(Ordering::Relaxed) < THRESHOLD
}

/// A frame count from a switch: `v` if it is a whole number above 0, else
/// `default`. On 2026-11-27 a loading screen of 5.7 s frames bound 100+
/// targets for 3 frames, and fast boot ended there, before the world
/// (`SCARLETT_FAST_BOOT_SUSTAIN`); with 30 world frames in a row asked for,
/// two runs in the world at 4,000+ draws a frame went minutes without them,
/// light frames coming between (`SCARLETT_FAST_BOOT_GAP`).
fn frames_or(v: Option<String>, default: u32) -> u32 {
    v.and_then(|v| v.trim().parse().ok()).filter(|&n| n > 0).unwrap_or(default)
}

/// Pure rule: the world run length after a frame with `binds` binds.
pub fn next_run(run: u32, binds: u32) -> u32 {
    next_world(run, 0, binds, 1).0
}

/// Pure rule: the world frames counted and the light frames in a row after
/// a frame with `binds` binds; `gap` light frames in a row start the count
/// again.
fn next_world(run: u32, light: u32, binds: u32, gap: u32) -> (u32, u32) {
    if binds >= THRESHOLD {
        (run + 1, 0)
    } else if light + 1 >= gap {
        (0, 0)
    } else {
        (run, light + 1)
    }
}

/// A frame was presented: updates the detector; returns the log line once,
/// when fast boot ends.
pub fn frame_presented() -> Option<String> {
    if unpaced_loading() && !active() {
        LAST_BINDS.store(BINDS.swap(0, Ordering::Relaxed), Ordering::Relaxed);
        return None;
    }
    if !active() {
        return None;
    }
    let frame = FRAME.fetch_add(1, Ordering::Relaxed) + 1;
    let binds = BINDS.swap(0, Ordering::Relaxed);
    LAST_BINDS.store(binds, Ordering::Relaxed);
    let gap = *GAP_FRAMES.get_or_init(|| frames_or(env("SCARLETT_FAST_BOOT_GAP"), 1));
    let (run, light) = next_world(RUN.load(Ordering::Relaxed), LIGHT.load(Ordering::Relaxed), binds, gap);
    RUN.store(run, Ordering::Relaxed);
    LIGHT.store(light, Ordering::Relaxed);
    let heavy = HEAVY.fetch_add(u32::from(binds >= THRESHOLD), Ordering::Relaxed) + u32::from(binds >= THRESHOLD);
    if frame % 30 == 0 {
        HEAVY.store(0, Ordering::Relaxed);
        tracing::info!(frame, binds, run, light, heavy, "fast boot: still booting (world frames of the last 30)");
    }
    let max = *MAX.get_or_init(|| {
        env("SCARLETT_FAST_BOOT_MAX_FRAMES").and_then(|v| v.parse().ok()).unwrap_or(u64::MAX)
    });
    let max_s = *MAX_S.get_or_init(|| {
        env("SCARLETT_FAST_BOOT_MAX_S").and_then(|v| v.parse().ok()).unwrap_or(MAX_SECONDS)
    });
    let elapsed = START.get().map_or(0, |s| s.elapsed().as_secs());
    let sustain = *SUSTAIN_FRAMES.get_or_init(|| frames_or(env("SCARLETT_FAST_BOOT_SUSTAIN"), SUSTAIN));
    let why = if run >= sustain {
        "render targets"
    } else if frame >= max {
        "frame limit"
    } else if elapsed >= max_s {
        "time limit"
    } else {
        return None;
    };
    if IN_WORLD.swap(true, Ordering::Relaxed) {
        return None;
    }
    WORLD_AT.get_or_init(Instant::now);
    let secs = START.get().map_or(0.0, |s| s.elapsed().as_secs_f64());
    Some(format!("fast boot: in world after {frame} frames, {secs:.1} s ({why})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn world_needs_sustained_many_binds() {
        let mut run = 0;
        for binds in [3, 8, 5] {
            run = next_run(run, binds);
        }
        assert_eq!(run, 0);
        run = next_run(run, 200);
        run = next_run(run, 12);
        assert_eq!(run, 0);
        for _ in 0..SUSTAIN {
            run = next_run(run, 200);
        }
        assert!(run >= SUSTAIN);
    }

    #[test]
    fn frame_counts_are_asked_for_or_the_default() {
        assert_eq!(frames_or(None, SUSTAIN), SUSTAIN);
        assert_eq!(frames_or(Some(" 30 ".into()), SUSTAIN), 30);
        assert_eq!(frames_or(Some("0".into()), SUSTAIN), SUSTAIN);
        assert_eq!(frames_or(Some("many".into()), 1), 1);
    }

    #[test]
    fn light_frames_under_the_gap_keep_the_count() {
        // World frames with two light ones between each: gap 3 keeps counting.
        let (mut run, mut light) = (0, 0);
        for binds in [200, 4, 4, 200, 4, 4, 200] {
            (run, light) = next_world(run, light, binds, 3);
        }
        assert_eq!(run, 3);
        // Three light frames in a row start again.
        for _ in 0..3 {
            (run, light) = next_world(run, light, 4, 3);
        }
        assert_eq!((run, light), (0, 0));
        // Gap 1 is the old rule: any light frame starts again.
        assert_eq!(next_world(5, 0, 4, 1), (0, 0));
    }

    #[test]
    fn switch_parsing() {
        assert!(!truthy(None));
        assert!(!truthy(Some("0".into())));
        assert!(truthy(Some("1".into())));
    }
}
