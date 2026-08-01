//! `seize` on a freestanding Cortex-M4, run under qemu.
//!
//! No operating system, no host lock registered — this is the spin-latch
//! configuration, on a target where the thumbv7em atomics actually execute.
//! Success and failure are reported through the semihosting exit code, which
//! qemu forwards as its own.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;
use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering};

use cortex_m_rt::entry;
use cortex_m_semihosting::debug;
use seize::lock::Lock;
use seize::{Collector, Guard, reclaim};

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    debug::exit(debug::EXIT_FAILURE);
    loop {}
}

// ---------------------------------------------------------------------------
// A bump allocator over a static arena
// ---------------------------------------------------------------------------

const ARENA_SIZE: usize = 512 << 10;

#[repr(C, align(8))]
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

const RETIRED: usize = 256;

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

#[entry]
fn main() -> ! {
    checks();

    // No host lock was ever registered, so the first lock latched the spin
    // fallback, and a late registration must be refused.
    unsafe extern "C" fn acquire(_key: *const c_void) {}
    unsafe extern "C" fn release(_key: *const c_void) {}
    static LATE: Lock = Lock { acquire, release };
    assert!(!seize::lock::register(&LATE));

    debug::exit(debug::EXIT_SUCCESS);
    loop {}
}
