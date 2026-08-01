//! Synchronization primitives, backed by the operating system where there is
//! one.
//!
//! With the `std` feature enabled this is a thin wrapper around `std::sync`.
//! Without it, all locking is delegated to [`crate::lock`]: the host's
//! registered implementation, or a test-and-set spin lock if there is none.

#[cfg(feature = "std")]
pub use self::std_impl::*;

#[cfg(not(feature = "std"))]
pub use self::nostd::*;

#[cfg(feature = "std")]
mod std_impl {
    pub use std::sync::MutexGuard;

    /// A mutual exclusion primitive.
    ///
    /// Poisoning is propagated as a panic, as every critical section in this
    /// crate leaves shared state consistent only if it runs to completion.
    #[derive(Default)]
    pub struct Mutex<T>(std::sync::Mutex<T>);

    impl<T> Mutex<T> {
        #[inline]
        pub const fn new(value: T) -> Mutex<T> {
            Mutex(std::sync::Mutex::new(value))
        }

        #[inline]
        pub fn lock(&self) -> MutexGuard<'_, T> {
            self.0.lock().expect("seize: poisoned lock")
        }
    }
}

#[cfg(not(feature = "std"))]
mod nostd {
    use core::cell::UnsafeCell;
    use core::ops::{Deref, DerefMut};
    use core::sync::atomic::AtomicBool;

    use crate::lock;

    /// A mutual exclusion primitive.
    ///
    /// This type holds no protocol of its own: it pairs a region of memory
    /// with the word that identifies its critical section, and delegates the
    /// locking itself to [`crate::lock`]. The word doubles as the state of
    /// the fallback spin lock when the host has not registered anything.
    pub struct Mutex<T> {
        state: AtomicBool,
        value: UnsafeCell<T>,
    }

    // Safety: The lock guarantees unique access to the value.
    unsafe impl<T: Send> Send for Mutex<T> {}
    unsafe impl<T: Send> Sync for Mutex<T> {}

    impl<T: Default> Default for Mutex<T> {
        fn default() -> Mutex<T> {
            Mutex::new(T::default())
        }
    }

    impl<T> Mutex<T> {
        #[inline]
        pub const fn new(value: T) -> Mutex<T> {
            Mutex {
                state: AtomicBool::new(false),
                value: UnsafeCell::new(value),
            }
        }

        #[inline]
        pub fn lock(&self) -> MutexGuard<'_, T> {
            lock::acquire(&self.state);
            MutexGuard { mutex: self }
        }
    }

    pub struct MutexGuard<'a, T> {
        mutex: &'a Mutex<T>,
    }

    impl<T> Deref for MutexGuard<'_, T> {
        type Target = T;

        #[inline]
        fn deref(&self) -> &T {
            // Safety: We hold the lock.
            unsafe { &*self.mutex.value.get() }
        }
    }

    impl<T> DerefMut for MutexGuard<'_, T> {
        #[inline]
        fn deref_mut(&mut self) -> &mut T {
            // Safety: We hold the lock.
            unsafe { &mut *self.mutex.value.get() }
        }
    }

    impl<T> Drop for MutexGuard<'_, T> {
        #[inline]
        fn drop(&mut self) {
            lock::release(&self.mutex.state);
        }
    }
}

// These tests drive the lock delegation with a blocking host lock. Under
// `std` the locks are `std::sync` locks and there is nothing here to test,
// so they only run under `cargo test --no-default-features`. The spin
// fallback is covered separately: the integration tests never register a
// lock.
#[cfg(all(test, not(feature = "std")))]
mod tests {
    use super::*;
    use crate::lock::{self, Lock};

    use core::ffi::c_void;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex as StdMutex, Once};
    use std::thread;

    /// How many times the host lock was entered.
    static ACQUIRES: AtomicUsize = AtomicUsize::new(0);

    // A single global blocking lock, built as a binary semaphore because a
    // `MutexGuard` cannot be carried between the two extern calls. Ignoring
    // the key is explicitly allowed by the contract.
    static HELD: StdMutex<bool> = StdMutex::new(false);
    static RELEASED: Condvar = Condvar::new();

    unsafe extern "C" fn acquire(_key: *const c_void) {
        ACQUIRES.fetch_add(1, Ordering::Relaxed);

        let mut held = HELD.lock().unwrap();
        while *held {
            held = RELEASED.wait(held).unwrap();
        }
        *held = true;
    }

    unsafe extern "C" fn release(_key: *const c_void) {
        *HELD.lock().unwrap() = false;
        RELEASED.notify_one();
    }

    fn install_lock() {
        static LOCK: Lock = Lock { acquire, release };
        static ONCE: Once = Once::new();
        ONCE.call_once(|| assert!(lock::register(&LOCK)));
    }

    #[test]
    fn mutex_excludes() {
        install_lock();

        const THREADS: usize = 4;
        const OPS: usize = if cfg!(miri) { 100 } else { 10_000 };

        let counter = Mutex::new(0_u64);

        thread::scope(|s| {
            for _ in 0..THREADS {
                s.spawn(|| {
                    for _ in 0..OPS {
                        *counter.lock() += 1;
                    }
                });
            }
        });

        assert_eq!(*counter.lock(), (THREADS * OPS) as u64);
        assert!(
            ACQUIRES.load(Ordering::Relaxed) > 0,
            "the host lock was bypassed"
        );
    }

    #[test]
    fn registration_is_first_wins() {
        install_lock();

        static SECOND: Lock = Lock { acquire, release };
        assert!(!lock::register(&SECOND));
    }
}
