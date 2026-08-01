//! A `no_std`, no-libc Linux binary exercising `seize` end to end.
//!
//! Nothing here comes from a C runtime: startup is a hand-written `_start`,
//! the allocator is a bump allocator over a static arena, `mem*` are defined
//! below, and the host lock registered with [`seize::lock`] makes real futex
//! syscalls through `rustix`, whose `linux_raw` backend needs no libc
//! either. CI additionally asserts that the binary has no dynamic linkage.
//!
//! The process communicates through its exit code alone:
//!
//! - `0`  — every check passed
//! - `2`  — the initial lock registration was refused
//! - `3`  — the host lock was never used
//! - `4`  — a second registration was wrongly accepted
//! - `101` — a check failed (panics land here)

#![no_std]
#![no_main]
// Keep LLVM from recognizing the bodies of `memcpy` and friends below and
// "optimizing" them into calls to themselves.
#![no_builtins]

extern crate alloc;

use alloc::boxed::Box;
use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use seize::lock::Lock;
use seize::{Collector, Guard, reclaim};

// ---------------------------------------------------------------------------
// Startup and shutdown
// ---------------------------------------------------------------------------

core::arch::global_asm!(
    ".global _start",
    "_start:",
    // The ABI wants a zeroed frame pointer and a stack aligned to 16 bytes
    // before the call pushes the return address.
    "xor ebp, ebp",
    "and rsp, -16",
    "call main_nolibc",
);

fn exit(code: i32) -> ! {
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rax") 231_usize, // exit_group
            in("rdi") code,
            options(noreturn, nostack),
        )
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(101)
}

/// Referenced by the prebuilt `alloc` rlib, which was compiled with
/// unwinding. This binary aborts on panic, so it is never called.
#[unsafe(no_mangle)]
extern "C" fn rust_eh_personality() {}

// ---------------------------------------------------------------------------
// The `mem*` functions libc would otherwise provide
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
unsafe extern "C" fn memcpy(dest: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    for i in 0..n {
        unsafe { dest.add(i).write(src.add(i).read()) };
    }
    dest
}

#[unsafe(no_mangle)]
unsafe extern "C" fn memmove(dest: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    if (dest as usize) < (src as usize) {
        for i in 0..n {
            unsafe { dest.add(i).write(src.add(i).read()) };
        }
    } else {
        for i in (0..n).rev() {
            unsafe { dest.add(i).write(src.add(i).read()) };
        }
    }
    dest
}

#[unsafe(no_mangle)]
unsafe extern "C" fn memset(dest: *mut u8, byte: i32, n: usize) -> *mut u8 {
    for i in 0..n {
        unsafe { dest.add(i).write(byte as u8) };
    }
    dest
}

#[unsafe(no_mangle)]
unsafe extern "C" fn memcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    for i in 0..n {
        let (x, y) = unsafe { (a.add(i).read(), b.add(i).read()) };
        if x != y {
            return x as i32 - y as i32;
        }
    }
    0
}

#[unsafe(no_mangle)]
unsafe extern "C" fn bcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    unsafe { memcmp(a, b, n) }
}

// ---------------------------------------------------------------------------
// A bump allocator over a static arena
// ---------------------------------------------------------------------------

const ARENA_SIZE: usize = 4 << 20;

#[repr(C, align(64))]
struct Arena(UnsafeCell<[u8; ARENA_SIZE]>);

// Safety: The allocator below hands out each region at most once.
unsafe impl Sync for Arena {}

static ARENA: Arena = Arena(UnsafeCell::new([0; ARENA_SIZE]));

/// Never frees; the test is over before that matters. Reclamation is instead
/// observed through drop counts.
struct BumpAlloc {
    used: AtomicUsize,
}

unsafe impl GlobalAlloc for BumpAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let base = ARENA.0.get() as usize;
        let mut used = self.used.load(Ordering::Relaxed);

        loop {
            let start = (base + used).next_multiple_of(layout.align());
            let end = start + layout.size() - base;

            if end > ARENA_SIZE {
                return core::ptr::null_mut();
            }

            match self
                .used
                .compare_exchange_weak(used, end, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return start as *mut u8,
                Err(current) => used = current,
            }
        }
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}

#[global_allocator]
static HEAP: BumpAlloc = BumpAlloc {
    used: AtomicUsize::new(0),
};

// ---------------------------------------------------------------------------
// The host lock: a futex through rustix, no libc involved
// ---------------------------------------------------------------------------

static ACQUIRES: AtomicUsize = AtomicUsize::new(0);
static WAKES: AtomicUsize = AtomicUsize::new(0);

/// One global lock word, which the contract explicitly allows.
static WORD: AtomicU32 = AtomicU32::new(0);

unsafe extern "C" fn acquire(_key: *const c_void) {
    ACQUIRES.fetch_add(1, Ordering::Relaxed);

    while WORD
        .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        // This binary is single-threaded, so the lock is never actually
        // contended and this never sleeps; it is here so that a contended
        // build of this harness would still be correct.
        let _ = rustix::thread::futex::wait(&WORD, rustix::thread::futex::Flags::PRIVATE, 1, None);
    }
}

unsafe extern "C" fn release(_key: *const c_void) {
    WORD.store(0, Ordering::Release);

    // This lock does not track waiters, so it must always wake. It also
    // proves the raw syscall path works: every release goes through the
    // kernel.
    if rustix::thread::futex::wake(&WORD, rustix::thread::futex::Flags::PRIVATE, 1).is_ok() {
        WAKES.fetch_add(1, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// The checks
// ---------------------------------------------------------------------------

static DROPS: AtomicUsize = AtomicUsize::new(0);

/// A value that counts its own drop; padded so boxing it exercises the
/// allocator.
struct Counted {
    _pad: [u8; 64],
}

impl Counted {
    fn new() -> Counted {
        Counted { _pad: [0; 64] }
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        DROPS.fetch_add(1, Ordering::Relaxed);
    }
}

const RETIRED: usize = 512;

fn checks() {
    let collector = Collector::new().unwrap();

    {
        let guard = collector.enter_owned().unwrap();

        for _ in 0..RETIRED {
            let ptr = Box::into_raw(Box::new(Counted::new()));

            // Reserve first, so the retirement itself cannot fail.
            guard.reserve_retire().unwrap();

            // Safety: `ptr` was just allocated with `Box` and is unreachable
            // to anyone else.
            unsafe { guard.defer_retire(ptr, reclaim::boxed::<Counted>).unwrap() };
        }

        // Nothing can be reclaimed while the guard is active.
        assert_eq!(DROPS.load(Ordering::Relaxed), 0);

        guard.flush();
    }

    // Dropping the collector reclaims everything that was retired.
    drop(collector);
    assert_eq!(DROPS.load(Ordering::Relaxed), RETIRED);
}

#[unsafe(no_mangle)]
extern "C" fn main_nolibc() -> ! {
    static LOCK: Lock = Lock { acquire, release };

    if !seize::lock::register(&LOCK) {
        exit(2);
    }

    checks();

    // Everything above went through the registered lock, so it must have
    // been entered, and every release wakes through a real futex syscall.
    if ACQUIRES.load(Ordering::Relaxed) == 0 || WAKES.load(Ordering::Relaxed) == 0 {
        exit(3);
    }

    // A second registration must be refused.
    static SECOND: Lock = Lock { acquire, release };
    if seize::lock::register(&SECOND) {
        exit(4);
    }

    exit(0)
}
