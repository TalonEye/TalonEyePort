//! Calls in the title's code sent to functions of ours: a `call rel32` at a known place becomes a
//! call to a stub near the image that jumps on to the function given, which may call the title's
//! own function itself (the target the call had, which [`redirect_call`] returns).
//!
//! The places are offsets in one build of a title's image. A place is changed only when the
//! instruction there is a `call rel32` to the target expected, so another build of the image, in
//! which the offsets name other code, is left as it is.

use std::sync::Mutex;

/// Why a call was not redirected.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// The place is not inside the image.
    Outside,
    /// The bytes there are not a `call rel32` to the target expected (another build).
    NotTheCall,
    /// No memory for the stub within reach of the place, or the stubs' memory is full.
    NoRoom,
    /// The page of the place could not be made writable.
    Protect,
}

/// The stubs' memory near the image, and the bytes of it used.
static STUBS: Mutex<Option<(usize, usize)>> = Mutex::new(None);

/// `mov rax, to; jmp rax`. At a call `rax` holds nothing the callee reads.
fn stub(to: usize) -> [u8; 12] {
    let mut code = [0u8; 12];
    code[..2].copy_from_slice(&[0x48, 0xb8]);
    code[2..10].copy_from_slice(&(to as u64).to_le_bytes());
    code[10..].copy_from_slice(&[0xff, 0xe0]);
    code
}

/// The target of the `call rel32` in `code` (five bytes at `at`), if it is one.
fn call_target(code: [u8; 5], at: usize) -> Option<usize> {
    if code[0] != 0xe8 {
        return None;
    }
    let rel = i32::from_le_bytes([code[1], code[2], code[3], code[4]]) as i64;
    Some((at as i64 + 5 + rel) as usize)
}

/// `call rel32` at `at` to `to`, if it reaches.
fn call(at: usize, to: usize) -> Option<[u8; 5]> {
    let rel = i32::try_from(to as i64 - (at as i64 + 5)).ok()?;
    let [a, b, c, d] = rel.to_le_bytes();
    Some([0xe8, a, b, c, d])
}

/// Makes the `call rel32` at `base + offset`, which must call `base + target`, call `ours`
/// instead; returns the address of the title's function it called.
///
/// # Safety
/// `base` and `size` describe a mapped image, and no thread runs the place's code yet (or can be
/// inside its five bytes). `ours` takes the arguments the title's function takes, the same way.
pub unsafe fn redirect_call(
    base: usize,
    size: usize,
    offset: usize,
    target: usize,
    ours: usize,
) -> Result<usize, Refused> {
    use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
    use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    if offset.checked_add(5).is_none_or(|end| end > size) || target >= size {
        return Err(Refused::Outside);
    }
    let at = base + offset;
    // SAFETY: five bytes inside the mapped image.
    let code: [u8; 5] = unsafe { std::ptr::read_unaligned(at as *const _) };
    if call_target(code, at) != Some(base + target) {
        return Err(Refused::NotTheCall);
    }
    let mut stubs = STUBS.lock().unwrap_or_else(|e| e.into_inner());
    if stubs.is_none() {
        *stubs = crate::console_insn::near(at).map(|place| (place, 0));
    }
    let Some((place, used)) = stubs.as_mut() else {
        return Err(Refused::NoRoom);
    };
    let to = *place + *used;
    let body = stub(ours);
    let (Some(new), true) = (call(at, to), *used + 16 <= crate::console_insn::STUBS_LEN) else {
        return Err(Refused::NoRoom);
    };
    let mut old = 0;
    // SAFETY: our own code memory, which nothing runs yet; then the five bytes of the call, whose
    // page is made writable for them and put back, and which nothing runs either.
    unsafe {
        std::ptr::copy_nonoverlapping(body.as_ptr(), to as *mut u8, body.len());
        if VirtualProtect(at as *const _, 5, PAGE_EXECUTE_READWRITE, &mut old) == 0 {
            return Err(Refused::Protect);
        }
        std::ptr::copy_nonoverlapping(new.as_ptr(), at as *mut u8, 5);
        VirtualProtect(at as *const _, 5, old, &mut old);
        FlushInstructionCache(GetCurrentProcess(), at as *const _, 5);
    }
    *used += 16;
    Ok(base + target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn call_bytes_round_trip() {
        let at = 0x1_4000_0000usize;
        let to = at - 0x47_77c0 + 5;
        let bytes = call(at, to).unwrap();
        assert_eq!(call_target(bytes, at), Some(to));
        assert_eq!(call_target([0xe9, 0, 0, 0, 0], at), None);
        assert!(call(at, at + (5 << 30)).is_none());
    }

    static ORIGINAL: AtomicUsize = AtomicUsize::new(0);

    extern "system" fn theirs(a: u64, b: u64) -> u64 {
        a * 10 + b
    }

    extern "system" fn ours(a: u64, b: u64) -> u64 {
        // SAFETY: the address `redirect_call` returned, of `theirs`.
        let f: extern "system" fn(u64, u64) -> u64 =
            unsafe { std::mem::transmute(ORIGINAL.load(Ordering::SeqCst)) };
        f(a, b) + 1000
    }

    /// A call in code made for the test goes to ours, which calls theirs; a place whose call goes
    /// elsewhere is refused and left as it was.
    #[test]
    fn a_redirected_call_reaches_ours_and_theirs() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE, VirtualAlloc,
        };
        // SAFETY: fresh memory made for the test's code.
        let image = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                0x1000,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_EXECUTE_READWRITE,
            )
        } as usize;
        assert_ne!(image, 0);
        // At 0x100, theirs within the image: mov rax, theirs; jmp rax. At 0: call 0x100; ret.
        let thunk = stub(theirs as *const () as usize);
        let entry = call(image, image + 0x100).unwrap();
        // SAFETY: inside the memory made above.
        unsafe {
            std::ptr::copy_nonoverlapping(thunk.as_ptr(), (image + 0x100) as *mut u8, thunk.len());
            std::ptr::copy_nonoverlapping(entry.as_ptr(), image as *mut u8, 5);
            *((image + 5) as *mut u8) = 0xc3;
        }
        let run: extern "system" fn(u64, u64) -> u64 = unsafe { std::mem::transmute(image) };
        assert_eq!(run(4, 2), 42);
        // SAFETY: the image above, which no thread runs meanwhile.
        assert_eq!(
            unsafe { redirect_call(image, 0x1000, 0, 0x200, ours as *const () as usize) },
            Err(Refused::NotTheCall)
        );
        assert_eq!(
            unsafe { redirect_call(image, 0x1000, 0xffe, 0x100, ours as *const () as usize) },
            Err(Refused::Outside)
        );
        assert_eq!(run(4, 2), 42);
        let original =
            unsafe { redirect_call(image, 0x1000, 0, 0x100, ours as *const () as usize) }.unwrap();
        assert_eq!(original, image + 0x100);
        ORIGINAL.store(original, Ordering::SeqCst);
        assert_eq!(run(4, 2), 1042);
    }
}
