//! Tests for the API surface that is available without an operating system.
//!
//! Everything here goes through [`OwnedGuard`], which owns its thread slot
//! rather than looking one up in thread-local storage. That is the whole of
//! the crate under `--no-default-features`. No host lock is ever registered
//! in this binary, so under that configuration these tests also cover the
//! spin fallback of `seize::lock`.
//!
//! The test harness itself always links `std`, so these can still use threads.

use seize::{Collector, Guard, OwnedGuard, reclaim};

use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

/// A value that counts its own drop.
struct Counted(Arc<AtomicUsize>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

struct Node {
    next: *mut Node,
    // Only ever read by its own drop glue, which is the point: it is how a
    // reclaimed node makes itself visible to the test.
    #[allow(dead_code)]
    value: Counted,
}

/// A Treiber stack, retiring popped nodes through the guard they were popped
/// with.
struct Stack {
    head: AtomicPtr<Node>,
    collector: Collector,
}

impl Stack {
    fn new() -> Stack {
        Stack {
            head: AtomicPtr::new(ptr::null_mut()),
            // A batch size of one forces a retirement to be attempted on every
            // pop, which is what puts pressure on the reservation lock.
            collector: Collector::new().unwrap().batch_size(1),
        }
    }

    fn push(&self, value: Counted, guard: &impl Guard) {
        let new = Box::into_raw(Box::new(Node {
            next: ptr::null_mut(),
            value,
        }));

        loop {
            let head = guard.protect(&self.head, Ordering::Relaxed);

            // Safety: `new` is owned by this thread until it is published below.
            unsafe { (*new).next = head };

            if self
                .head
                .compare_exchange(head, new, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
        }
    }

    /// Pops a value, returning `false` if the stack was empty.
    ///
    /// The node is retired with a reservation taken beforehand, so the
    /// retirement itself cannot fail — which matters because by then the node
    /// is already unlinked and there is nowhere to report an error.
    fn pop(&self, guard: &impl Guard) -> bool {
        loop {
            let head = guard.protect(&self.head, Ordering::Acquire);

            if head.is_null() {
                return false;
            }

            // Safety: `head` is protected by the guard.
            let next = unsafe { (*head).next };

            let reserved = guard.reserve_retire().is_ok();

            if self
                .head
                .compare_exchange(head, next, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                // Safety: We unlinked the node, so no thread that enters from
                // here on can reach it, and it was allocated with `Box`.
                let retired = unsafe { guard.defer_retire(head, reclaim::boxed::<Node>) };

                // The reservation guarantees the retirement cannot fail.
                assert!(!reserved || retired.is_ok());
                return true;
            }
        }
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        let mut node = *self.head.get_mut();

        while !node.is_null() {
            // Safety: We have `&mut self`, so no other thread is accessing the
            // stack, and every node was allocated with `Box`.
            unsafe {
                let next = (*node).next;
                drop(Box::from_raw(node));
                node = next;
            }
        }
    }
}

/// The locking mode latches the first time a lock is taken, so a host lock
/// arriving after seize has already run must be refused: honoring it would
/// split the world between threads that spin and threads that block. This
/// binary never registers a lock up front, which also makes it the coverage
/// for the spin fallback itself.
#[cfg(not(feature = "std"))]
#[test]
fn late_lock_registration_is_refused() {
    use core::ffi::c_void;

    unsafe extern "C" fn acquire(_key: *const c_void) {}
    unsafe extern "C" fn release(_key: *const c_void) {}

    // Taking a guard allocates a thread slot, which takes a lock and latches
    // spin mode.
    let collector = Collector::new().unwrap();
    drop(collector.enter_owned().unwrap());

    static LOCK: seize::lock::Lock = seize::lock::Lock { acquire, release };
    assert!(!seize::lock::register(&LOCK));
}

#[test]
fn owned_guard_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Collector>();
    assert_send_sync::<OwnedGuard<'_>>();
}

/// Values retired through an owned guard are reclaimed once it is dropped.
#[test]
fn retires_through_an_owned_guard() {
    const ITEMS: usize = if cfg!(miri) { 32 } else { 128 };

    let dropped = Arc::new(AtomicUsize::new(0));
    let stack = Stack::new();

    {
        let guard = stack.collector.enter_owned().unwrap();

        for _ in 0..ITEMS {
            stack.push(Counted(dropped.clone()), &guard);
        }

        for _ in 0..ITEMS {
            assert!(stack.pop(&guard));
        }

        assert!(!stack.pop(&guard));

        // Nothing can be reclaimed while the guard is active.
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
    }

    // Dropping the collector reclaims whatever the guard was holding back.
    drop(stack);
    assert_eq!(dropped.load(Ordering::Relaxed), ITEMS);
}

/// A guard per worker, which is the arrangement a freestanding host is expected
/// to use. Each `enter_owned` and drop allocates and frees a thread slot, so
/// this also hammers the thread ID manager's lock.
#[test]
fn one_guard_per_worker() {
    const WORKERS: usize = 4;
    const ROUNDS: usize = if cfg!(miri) { 2 } else { 8 };
    const ITEMS: usize = if cfg!(miri) { 16 } else { 512 };

    let dropped = Arc::new(AtomicUsize::new(0));
    let popped = AtomicUsize::new(0);
    let stack = Stack::new();
    let barrier = Barrier::new(WORKERS);

    thread::scope(|s| {
        for _ in 0..WORKERS {
            let (stack, dropped, barrier, popped) = (&stack, &dropped, &barrier, &popped);

            s.spawn(move || {
                barrier.wait();

                for _ in 0..ROUNDS {
                    // Taken and dropped per round, to churn thread slots.
                    let guard = stack.collector.enter_owned().unwrap();

                    for _ in 0..ITEMS {
                        stack.push(Counted(dropped.clone()), &guard);
                    }

                    for _ in 0..ITEMS {
                        if stack.pop(&guard) {
                            popped.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            });
        }
    });

    // A worker can find the stack transiently empty, so not every pop
    // succeeds. What matters is that reclamation ran while the workers did.
    assert!(popped.load(Ordering::Relaxed) > 0);
    assert!(
        dropped.load(Ordering::Relaxed) > 0,
        "retired values should have been reclaimed as guards were dropped"
    );

    // Whatever is left on the stack is freed directly, so the totals still add
    // up: every value pushed is accounted for exactly once.
    drop(stack);
    assert_eq!(
        dropped.load(Ordering::Relaxed),
        WORKERS * ROUNDS * ITEMS,
        "every value should have been reclaimed"
    );
}

/// A single owned guard shared between workers. `OwnedGuard` is `Sync`, and
/// this is what its internal lock exists to serialize.
#[test]
fn one_guard_shared_between_workers() {
    const WORKERS: usize = 4;
    const ITEMS: usize = if cfg!(miri) { 16 } else { 512 };

    let dropped = Arc::new(AtomicUsize::new(0));
    let stack = Stack::new();
    let barrier = Barrier::new(WORKERS);

    {
        let guard = stack.collector.enter_owned().unwrap();

        thread::scope(|s| {
            for _ in 0..WORKERS {
                let (stack, dropped, barrier, guard) = (&stack, &dropped, &barrier, &guard);

                s.spawn(move || {
                    barrier.wait();

                    for _ in 0..ITEMS {
                        stack.push(Counted(dropped.clone()), guard);
                    }

                    for _ in 0..ITEMS {
                        stack.pop(guard);
                    }

                    // Contend the same lock from another direction.
                    guard.flush();
                });
            }
        });

        // The guard held every retirement back for its whole lifetime.
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
    }

    drop(stack);
    assert_eq!(dropped.load(Ordering::Relaxed), WORKERS * ITEMS);
}

/// Thread slots are freed when their guard is dropped, and reused afterwards,
/// so a host that creates guards repeatedly does not grow the collector's
/// thread-local storage without bound.
///
/// The slot ids are process-global and the other tests in this binary hold a
/// handful concurrently, so this asserts boundedness rather than exact reuse:
/// without recycling, a thousand create/drop cycles would push the id well
/// past any plausible number of live slots.
#[test]
fn thread_slots_are_recycled() {
    const CYCLES: usize = if cfg!(miri) { 64 } else { 1024 };

    let collector = Collector::new().unwrap();

    for _ in 0..CYCLES {
        let guard = collector.enter_owned().unwrap();
        assert!(guard.thread_id() < 64, "the slot should have been reused");
    }
}
