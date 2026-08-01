//! Mutual exclusion for `no_std` targets, implemented by the host.
//!
//! Blocking a thread is an operating-system service, and without the `std`
//! feature `seize` does not assume an operating system — so rather than
//! shipping its own lock protocol, it asks the host for one. The host
//! registers an [`acquire`](Lock::acquire)/[`release`](Lock::release) pair
//! once at startup, and `seize`'s two internal locks — around thread slot
//! allocation, and around an [`OwnedGuard`](crate::OwnedGuard)'s batch
//! operations — go through it. A Zig host hands over a `std.Thread.Mutex` in
//! four lines; an RTOS hands over its native mutex and gets priority
//! inheritance for free.
//!
//! If nothing is registered by the time the first lock is taken, `seize`
//! latches onto a built-in test-and-set spin lock instead, and registration
//! is refused from then on (see [`register`]). Spinning is correct, and in
//! the intended arrangement of one owned guard per worker the locks are
//! never contended, so it costs nothing there — but a spinner burns CPU
//! whenever the lock holder is descheduled, and on a strictly
//! priority-scheduled single core it can deadlock outright. Registering a
//! real lock is always the better choice when the host has one.
//!
//! With the `std` feature enabled this module is present but never
//! consulted: the locks are `std::sync` locks.
//!
//! # The contract
//!
//! `acquire(key)` must provide mutual exclusion per `key` until the matching
//! `release(key)`, which is always called on the acquiring thread. `seize`
//! never nests these sections, so a single global mutex, ignoring the key,
//! is a valid implementation. The key is the stable address of the lock, so
//! an implementation that wants finer granularity can also shard by it —
//! worth knowing because one of the sections sits on the retirement path.
//! One warning for allocator authors: a section may call into the host
//! allocator (retiring can allocate a batch), so the registered lock must
//! not be one the allocator itself takes.
//!
//! # Examples
//!
//! Registering a lock. This one is a global test-and-set, standing in for a
//! real host mutex:
//!
//! ```rust
//! use core::ffi::c_void;
//! use core::sync::atomic::{AtomicBool, Ordering};
//! use seize::lock::Lock;
//!
//! static HELD: AtomicBool = AtomicBool::new(false);
//!
//! unsafe extern "C" fn acquire(_key: *const c_void) {
//!     while HELD.swap(true, Ordering::Acquire) {
//!         core::hint::spin_loop();
//!     }
//! }
//!
//! unsafe extern "C" fn release(_key: *const c_void) {
//!     HELD.store(false, Ordering::Release);
//! }
//!
//! static LOCK: Lock = Lock { acquire, release };
//!
//! assert!(seize::lock::register(&LOCK));
//! ```
//!
//! The functions use the C ABI so they can come from outside Rust entirely.
//! A Zig host, for example, hands over one of its own mutexes:
//!
//! ```text
//! // zig
//! const std = @import("std");
//!
//! var lock: std.Thread.Mutex = .{};
//!
//! export fn seize_acquire(key: ?*const anyopaque) void {
//!     _ = key;
//!     lock.lock();
//! }
//!
//! export fn seize_release(key: ?*const anyopaque) void {
//!     _ = key;
//!     lock.unlock();
//! }
//! ```
//!
//! ```rust,ignore
//! // The Rust side of the embedding, e.g. in a `staticlib` shim.
//! unsafe extern "C" {
//!     fn seize_acquire(key: *const core::ffi::c_void);
//!     fn seize_release(key: *const core::ffi::c_void);
//! }
//!
//! static LOCK: seize::lock::Lock = seize::lock::Lock {
//!     acquire: seize_acquire,
//!     release: seize_release,
//! };
//!
//! seize::lock::register(&LOCK);
//! ```

use core::ffi::c_void;
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicU8, Ordering};

#[cfg(not(feature = "std"))]
use core::hint::spin_loop;
#[cfg(not(feature = "std"))]
use core::sync::atomic::AtomicBool;

/// A host-provided lock implementation.
///
/// The struct is `#[repr(C)]` and the functions use the C ABI, so both can
/// be provided by a non-Rust embedder. See the [module
/// documentation](crate::lock) for the contract.
#[repr(C)]
pub struct Lock {
    /// Blocks until the section for `key` can be entered exclusively.
    pub acquire: unsafe extern "C" fn(key: *const c_void),

    /// Ends the section for `key`. Always called on the acquiring thread.
    pub release: unsafe extern "C" fn(key: *const c_void),
}

/// No lock has been taken and no host lock has been registered.
const UNSET: u8 = 0;

/// Every lock goes through the registered host lock.
const HOST: u8 = 1;

/// Every lock spins. Latched by the first acquire if no host lock was
/// registered by then.
#[cfg(not(feature = "std"))]
const SPIN: u8 = 2;

/// The locking mode, latched at first use.
static MODE: AtomicU8 = AtomicU8::new(UNSET);

/// The registered host lock, or null.
static LOCK: AtomicPtr<Lock> = AtomicPtr::new(ptr::null_mut());

/// Registers the lock used by `seize`'s internal critical sections.
///
/// Returns `true` if this lock is now in use. The locking mode is latched
/// the first time a lock is taken: if nothing was registered by then,
/// spinning is chosen permanently and this returns `false` — accepting a
/// host lock at that point would split the world between threads that spin
/// and threads that block, which is why late registration is refused rather
/// than honored. Register once at startup, before the first `Collector` is
/// created. A second registration also returns `false` and keeps the first.
///
/// With the `std` feature enabled, registration succeeds but has no effect,
/// as the locks are operating-system locks.
pub fn register(lock: &'static Lock) -> bool {
    // Publish the lock before latching the mode, so that a thread observing
    // `HOST` always observes the pointer.
    if LOCK
        .compare_exchange(
            ptr::null_mut(),
            lock as *const Lock as *mut Lock,
            Ordering::AcqRel,
            Ordering::Relaxed,
        )
        .is_err()
    {
        return false;
    }

    MODE.compare_exchange(UNSET, HOST, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

/// The latched locking mode, deciding it if this is the first use.
#[cfg(not(feature = "std"))]
fn mode() -> u8 {
    let mode = MODE.load(Ordering::Acquire);

    if mode != UNSET {
        return mode;
    }

    // The first lock is being taken and no host lock was registered: latch
    // spinning, unless a registration wins the race, in which case use it.
    match MODE.compare_exchange(UNSET, SPIN, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => SPIN,
        Err(mode) => mode,
    }
}

/// Enters the exclusive section identified by `state`.
#[cfg(not(feature = "std"))]
pub(crate) fn acquire(state: &AtomicBool) {
    if mode() == HOST {
        let lock = LOCK.load(Ordering::Acquire);

        // Safety: `HOST` is only latched after the pointer is published, and
        // the registrant promised the contract in the module documentation.
        unsafe { ((*lock).acquire)(state.as_ptr().cast::<c_void>().cast_const()) };
        return;
    }

    // Spin: a plain test-and-set lock.
    loop {
        if !state.swap(true, Ordering::Acquire) {
            return;
        }

        // Wait on a read-only load, to avoid taking the cache line
        // exclusively on every attempt.
        while state.load(Ordering::Relaxed) {
            spin_loop();
        }
    }
}

/// Leaves the exclusive section identified by `state`.
#[cfg(not(feature = "std"))]
pub(crate) fn release(state: &AtomicBool) {
    // A release only ever follows an acquire, so the mode is latched.
    if MODE.load(Ordering::Acquire) == HOST {
        let lock = LOCK.load(Ordering::Acquire);

        // Safety: As in `acquire`.
        unsafe { ((*lock).release)(state.as_ptr().cast::<c_void>().cast_const()) };
        return;
    }

    state.store(false, Ordering::Release);
}
