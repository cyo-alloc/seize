use seize::alloc::{AllocError, Allocator, Global, Layout};
use seize::{Collector, Guard, reclaim};

use std::mem::ManuallyDrop;
use std::ptr;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex, mpsc};
use std::thread;

#[test]
fn is_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Collector>();
    assert_send_sync::<Collector>();
}

/// The allocation counters shared with a `CountingAlloc`.
#[derive(Default)]
struct Counters {
    allocated: AtomicUsize,
    live: AtomicUsize,
}

/// An allocator that counts the memory it has allocated and not yet freed.
///
/// Note that `Allocator` is implemented for the handle, not the counters, as
/// `Collector::new_in` takes ownership of the allocator.
#[derive(Clone, Default)]
struct CountingAlloc(Arc<Counters>);

// Safety: All allocations are forwarded to the global allocator.
unsafe impl Allocator for CountingAlloc {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let ptr = Global.allocate(layout)?;
        self.0.allocated.fetch_add(layout.size(), Ordering::Relaxed);
        self.0.live.fetch_add(layout.size(), Ordering::Relaxed);
        Ok(ptr)
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        self.0.live.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { Global.deallocate(ptr, layout) }
    }
}

#[test]
fn custom_allocator() {
    let alloc = CountingAlloc::default();
    let collector = Collector::new_in(alloc.clone()).unwrap().batch_size(2);

    // Creating a collector allocates thread-local storage.
    assert!(alloc.0.allocated.load(Ordering::Relaxed) > 0);

    let dropped = Arc::new(AtomicUsize::new(0));

    {
        let guard = collector.enter().unwrap();

        // Retire enough values to allocate, and reclaim, a batch.
        for _ in 0..10 {
            let value = boxed(DropTrack(dropped.clone()));

            // Safety: The value was never shared, and was allocated with `Box`.
            unsafe { guard.defer_retire(value, reclaim::boxed).unwrap() };
        }
    }

    let allocated = alloc.0.allocated.load(Ordering::Relaxed);
    drop(collector);

    // All retired values were reclaimed.
    assert_eq!(dropped.load(Ordering::Relaxed), 10);

    // Everything the collector allocated was freed through the same allocator,
    // and nothing was allocated after it was dropped.
    assert_eq!(alloc.0.live.load(Ordering::Relaxed), 0);
    assert_eq!(alloc.0.allocated.load(Ordering::Relaxed), allocated);
}

/// An allocator that can be switched to fail every allocation.
#[derive(Clone, Default)]
struct FailingAlloc(Arc<AtomicBool>);

impl FailingAlloc {
    /// Set whether allocations should fail.
    fn set_failing(&self, failing: bool) {
        self.0.store(failing, Ordering::Relaxed);
    }
}

// Safety: Allocations are either forwarded to the global allocator or fail.
// Note that deallocation always succeeds.
unsafe impl Allocator for FailingAlloc {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        if self.0.load(Ordering::Relaxed) {
            return Err(AllocError);
        }

        Global.allocate(layout)
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        unsafe { Global.deallocate(ptr, layout) }
    }
}

#[test]
fn allocation_failure() {
    // A collector cannot be created if its thread-local storage cannot be
    // allocated.
    let alloc = FailingAlloc::default();
    alloc.set_failing(true);
    assert!(Collector::new_in(alloc).is_err());

    let alloc = FailingAlloc::default();
    let collector = Collector::new_in(alloc.clone()).unwrap().batch_size(2);
    let dropped = Arc::new(AtomicUsize::new(0));

    let guard = collector.enter().unwrap();

    // Retirement fails instead of aborting once the allocator runs dry.
    alloc.set_failing(true);
    let value = boxed(DropTrack(dropped.clone()));

    // Safety: The value was never shared, and was allocated with `Box`.
    assert!(unsafe { guard.defer_retire(value, reclaim::boxed) }.is_err());

    // The failed retirement left the pointer untouched, so we still own it.
    assert_eq!(dropped.load(Ordering::Relaxed), 0);

    // Flushing never allocates, so it succeeds even while the allocator is dry.
    guard.flush();

    // Retirement succeeds again once memory is available.
    alloc.set_failing(false);

    // Safety: The value was never shared, and was allocated with `Box`.
    unsafe { guard.defer_retire(value, reclaim::boxed) }.unwrap();
    drop(guard);

    // Dropping the collector reclaims the value without allocating.
    alloc.set_failing(true);
    drop(collector);
    assert_eq!(dropped.load(Ordering::Relaxed), 1);
}

#[test]
fn reserve_retire() {
    let alloc = FailingAlloc::default();
    let collector = Collector::new_in(alloc.clone()).unwrap().batch_size(1);
    let dropped = Arc::new(AtomicUsize::new(0));

    let guard = collector.enter().unwrap();

    // Reserve a retirement while memory is still available.
    guard.reserve_retire().unwrap();
    alloc.set_failing(true);

    // The reserved retirement succeeds despite the allocator being dry.
    let value = boxed(DropTrack(dropped.clone()));

    // Safety: The value was never shared, and was allocated with `Box`.
    unsafe { guard.defer_retire(value, reclaim::boxed) }.unwrap();

    // The batch was retired, so the next retirement has to allocate, and fails.
    let value = boxed(DropTrack(dropped.clone()));

    // Safety: The value was never shared, and was allocated with `Box`.
    assert!(unsafe { guard.defer_retire(value, reclaim::boxed) }.is_err());

    // The failed retirement left the value untouched, so we still own it.
    assert_eq!(dropped.load(Ordering::Relaxed), 0);

    // Safety: The retirement failed, so the value was never retired.
    drop(unsafe { Box::from_raw(value) });
    assert_eq!(dropped.load(Ordering::Relaxed), 1);

    // Dropping the guard reclaims the value that was retired.
    drop(guard);
    assert_eq!(dropped.load(Ordering::Relaxed), 2);
}

#[test]
fn reserve_retire_owned() {
    let alloc = FailingAlloc::default();
    let collector = Collector::new_in(alloc.clone()).unwrap().batch_size(1);
    let dropped = Arc::new(AtomicUsize::new(0));

    let guard = collector.enter_owned().unwrap();
    guard.reserve_retire().unwrap();
    alloc.set_failing(true);

    let value = boxed(DropTrack(dropped.clone()));

    // Safety: The value was never shared, and was allocated with `Box`.
    unsafe { guard.defer_retire(value, reclaim::boxed) }.unwrap();

    drop(guard);
    assert_eq!(dropped.load(Ordering::Relaxed), 1);
}

struct DropTrack(Arc<AtomicUsize>);

impl Drop for DropTrack {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

fn boxed<T>(value: T) -> *mut T {
    Box::into_raw(Box::new(value))
}

struct UnsafeSend<T>(T);
unsafe impl<T> Send for UnsafeSend<T> {}

#[test]
fn single_thread() {
    let collector = Arc::new(Collector::new().unwrap().batch_size(2));
    let dropped = Arc::new(AtomicUsize::new(0));

    // multiple of 2
    let items = cfg::ITEMS & !1;

    for _ in 0..items {
        let zero = AtomicPtr::new(boxed(DropTrack(dropped.clone())));

        {
            let guard = collector.enter().unwrap();
            let _ = guard.protect(&zero, Ordering::Relaxed);
        }

        {
            let guard = collector.enter().unwrap();
            let value = guard.protect(&zero, Ordering::Acquire);
            unsafe { collector.retire(value, reclaim::boxed).unwrap() }
        }
    }

    assert_eq!(dropped.load(Ordering::Relaxed), items);
}

#[test]
fn two_threads() {
    let collector = Arc::new(Collector::new().unwrap().batch_size(3));

    let a_dropped = Arc::new(AtomicUsize::new(0));
    let b_dropped = Arc::new(AtomicUsize::new(0));

    let (tx, rx) = mpsc::channel();

    let one = Arc::new(AtomicPtr::new(boxed(DropTrack(a_dropped.clone()))));

    let h = thread::spawn({
        let one = one.clone();
        let collector = collector.clone();

        move || {
            let guard = collector.enter().unwrap();
            let _value = guard.protect(&one, Ordering::Acquire);
            tx.send(()).unwrap();
            drop(guard);
            tx.send(()).unwrap();
        }
    });

    for _ in 0..2 {
        let zero = AtomicPtr::new(boxed(DropTrack(b_dropped.clone())));
        let guard = collector.enter().unwrap();
        let value = guard.protect(&zero, Ordering::Acquire);
        unsafe { collector.retire(value, reclaim::boxed).unwrap() }
    }

    rx.recv().unwrap(); // wait for thread to access value
    let guard = collector.enter().unwrap();
    let value = guard.protect(&one, Ordering::Acquire);
    unsafe { collector.retire(value, reclaim::boxed).unwrap() }

    rx.recv().unwrap(); // wait for thread to drop guard
    h.join().unwrap();

    drop(guard);

    assert_eq!(
        (
            b_dropped.load(Ordering::Acquire),
            a_dropped.load(Ordering::Acquire)
        ),
        (2, 1)
    );
}

#[test]
fn refresh() {
    let collector = Arc::new(Collector::new().unwrap().batch_size(3));

    let items = (0..cfg::ITEMS)
        .map(|i| AtomicPtr::new(boxed(i)))
        .collect::<Arc<[_]>>();

    let handles = (0..cfg::THREADS)
        .map(|_| {
            thread::spawn({
                let items = items.clone();
                let collector = collector.clone();

                move || {
                    let mut guard = collector.enter().unwrap();

                    for _ in 0..cfg::ITER {
                        for item in items.iter() {
                            let item = guard.protect(item, Ordering::Acquire);
                            unsafe { assert!(*item < cfg::ITEMS) }
                        }

                        guard.refresh();
                    }
                }
            })
        })
        .collect::<Vec<_>>();

    for i in 0..cfg::ITER {
        for item in items.iter() {
            let old = item.swap(Box::into_raw(Box::new(i)), Ordering::AcqRel);
            unsafe { collector.retire(old, reclaim::boxed).unwrap() }
        }
    }

    for handle in handles {
        handle.join().unwrap()
    }

    // cleanup
    for item in items.iter() {
        let old = item.swap(ptr::null_mut(), Ordering::Acquire);
        unsafe { collector.retire(old, reclaim::boxed).unwrap() }
    }
}

#[test]
fn recursive_retire() {
    struct Recursive {
        _value: usize,
        pointers: Vec<*mut usize>,
    }

    let collector = Collector::new().unwrap().batch_size(1);

    let ptr = boxed(Recursive {
        _value: 0,
        pointers: (0..cfg::ITEMS).map(boxed).collect(),
    });

    unsafe {
        collector
            .retire(ptr, |ptr: *mut Recursive, collector| {
                let value = Box::from_raw(ptr);

                for pointer in value.pointers {
                    collector.retire(pointer, reclaim::boxed).unwrap();

                    let mut guard = collector.enter().unwrap();
                    guard.flush();
                    guard.refresh();
                    drop(guard);
                }
            })
            .unwrap();

        collector.enter().unwrap().flush();
    }
}

#[test]
fn reclaim_all() {
    let collector = Collector::new().unwrap().batch_size(2);

    for _ in 0..cfg::ITER {
        let dropped = Arc::new(AtomicUsize::new(0));

        let items = (0..cfg::ITEMS)
            .map(|_| AtomicPtr::new(boxed(DropTrack(dropped.clone()))))
            .collect::<Vec<_>>();

        for item in items {
            unsafe {
                collector
                    .retire(item.load(Ordering::Relaxed), reclaim::boxed)
                    .unwrap()
            };
        }

        unsafe { collector.reclaim_all() };
        assert_eq!(dropped.load(Ordering::Relaxed), cfg::ITEMS);
    }
}

#[test]
fn recursive_retire_reclaim_all() {
    struct Recursive {
        _value: usize,
        pointers: Vec<*mut DropTrack>,
    }

    unsafe {
        let collector = Collector::new().unwrap().batch_size(cfg::ITEMS * 2);
        let dropped = Arc::new(AtomicUsize::new(0));

        let ptr = boxed(Recursive {
            _value: 0,
            pointers: (0..cfg::ITEMS)
                .map(|_| boxed(DropTrack(dropped.clone())))
                .collect(),
        });

        collector
            .retire(ptr, |ptr: *mut Recursive, collector| {
                let value = Box::from_raw(ptr);
                for pointer in value.pointers {
                    (*collector).retire(pointer, reclaim::boxed).unwrap();
                }
            })
            .unwrap();

        collector.reclaim_all();
        assert_eq!(dropped.load(Ordering::Relaxed), cfg::ITEMS);
    }
}

#[test]
fn defer_retire() {
    let collector = Collector::new().unwrap().batch_size(5);
    let dropped = Arc::new(AtomicUsize::new(0));

    let objects: Vec<_> = (0..30).map(|_| boxed(DropTrack(dropped.clone()))).collect();

    let guard = collector.enter().unwrap();

    for object in objects {
        unsafe { guard.defer_retire(object, reclaim::boxed).unwrap() }
        guard.flush();
    }

    // guard is still active
    assert_eq!(dropped.load(Ordering::Relaxed), 0);
    drop(guard);
    // now the objects should have been dropped
    assert_eq!(dropped.load(Ordering::Relaxed), 30);
}

#[test]
fn reentrant() {
    let collector = Arc::new(Collector::new().unwrap().batch_size(5));
    let dropped = Arc::new(AtomicUsize::new(0));

    let objects: UnsafeSend<Vec<_>> =
        UnsafeSend((0..5).map(|_| boxed(DropTrack(dropped.clone()))).collect());

    assert_eq!(dropped.load(Ordering::Relaxed), 0);

    let guard1 = collector.enter().unwrap();
    let guard2 = collector.enter().unwrap();
    let guard3 = collector.enter().unwrap();

    thread::spawn({
        let collector = collector.clone();

        move || {
            let guard = collector.enter().unwrap();
            for object in { objects }.0 {
                unsafe { guard.defer_retire(object, reclaim::boxed).unwrap() }
            }
        }
    })
    .join()
    .unwrap();

    assert_eq!(dropped.load(Ordering::Relaxed), 0);
    drop(guard1);
    assert_eq!(dropped.load(Ordering::Relaxed), 0);
    drop(guard2);
    assert_eq!(dropped.load(Ordering::Relaxed), 0);
    drop(guard3);
    assert_eq!(dropped.load(Ordering::Relaxed), 5);

    let dropped = Arc::new(AtomicUsize::new(0));

    let objects: UnsafeSend<Vec<_>> =
        UnsafeSend((0..5).map(|_| boxed(DropTrack(dropped.clone()))).collect());

    assert_eq!(dropped.load(Ordering::Relaxed), 0);

    let mut guard1 = collector.enter().unwrap();
    let mut guard2 = collector.enter().unwrap();
    let mut guard3 = collector.enter().unwrap();

    thread::spawn({
        let collector = collector.clone();

        move || {
            let guard = collector.enter().unwrap();
            for object in { objects }.0 {
                unsafe { guard.defer_retire(object, reclaim::boxed).unwrap() }
            }
        }
    })
    .join()
    .unwrap();

    assert_eq!(dropped.load(Ordering::Relaxed), 0);
    guard1.refresh();
    assert_eq!(dropped.load(Ordering::Relaxed), 0);
    drop(guard1);
    guard2.refresh();
    assert_eq!(dropped.load(Ordering::Relaxed), 0);
    drop(guard2);
    assert_eq!(dropped.load(Ordering::Relaxed), 0);
    guard3.refresh();
    assert_eq!(dropped.load(Ordering::Relaxed), 5);
}

#[test]
fn swap_stress() {
    for _ in 0..cfg::ITER {
        let collector = Collector::new().unwrap();
        let entries = [const { AtomicPtr::new(ptr::null_mut()) }; cfg::ITEMS];

        thread::scope(|s| {
            for _ in 0..cfg::THREADS {
                s.spawn(|| {
                    for i in 0..cfg::ITEMS {
                        let guard = collector.enter().unwrap();
                        let new = Box::into_raw(Box::new(i));
                        let old = guard.swap(&entries[i], new, Ordering::AcqRel);
                        if !old.is_null() {
                            unsafe { assert_eq!(*old, i) }
                            unsafe { guard.defer_retire(old, reclaim::boxed).unwrap() }
                        }
                    }
                });
            }
        });

        for i in 0..cfg::ITEMS {
            let val = entries[i].load(Ordering::Relaxed);
            let _ = unsafe { Box::from_raw(val) };
        }
    }
}

#[test]
fn cas_stress() {
    for _ in 0..cfg::ITER {
        let collector = Collector::new().unwrap();
        let entries = [const { AtomicPtr::new(ptr::null_mut()) }; cfg::ITEMS];

        thread::scope(|s| {
            for _ in 0..cfg::THREADS {
                s.spawn(|| {
                    for i in 0..cfg::ITEMS {
                        let guard = collector.enter().unwrap();
                        let new = Box::into_raw(Box::new(i));

                        loop {
                            let old = entries[i].load(Ordering::Relaxed);

                            let result = guard.compare_exchange(
                                &entries[i],
                                old,
                                new,
                                Ordering::AcqRel,
                                Ordering::Relaxed,
                            );

                            let Ok(old) = result else {
                                continue;
                            };

                            if !old.is_null() {
                                unsafe { assert_eq!(*old, i) }
                                unsafe { guard.defer_retire(old, reclaim::boxed).unwrap() }
                            }

                            break;
                        }
                    }
                });
            }
        });

        for i in 0..cfg::ITEMS {
            let val = entries[i].load(Ordering::Relaxed);
            let _ = unsafe { Box::from_raw(val) };
        }
    }
}

#[test]
fn owned_guard() {
    let collector = Collector::new().unwrap().batch_size(5);
    let dropped = Arc::new(AtomicUsize::new(0));

    let objects = UnsafeSend(
        (0..5)
            .map(|_| AtomicPtr::new(boxed(DropTrack(dropped.clone()))))
            .collect::<Vec<_>>(),
    );

    assert_eq!(dropped.load(Ordering::Relaxed), 0);

    thread::scope(|s| {
        let guard1 = collector.enter_owned().unwrap();

        let guard2 = collector.enter().unwrap();
        for object in objects.0.iter() {
            unsafe {
                guard2
                    .defer_retire(object.load(Ordering::Acquire), reclaim::boxed)
                    .unwrap()
            }
        }

        drop(guard2);

        // guard1 is still active
        assert_eq!(dropped.load(Ordering::Relaxed), 0);

        s.spawn(move || {
            for object in objects.0.iter() {
                let _ = unsafe { &*guard1.protect(object, Ordering::Relaxed) };
            }

            // guard1 is still active
            assert_eq!(dropped.load(Ordering::Relaxed), 0);

            drop(guard1);

            assert_eq!(dropped.load(Ordering::Relaxed), 5);
        });
    });
}

#[test]
fn owned_guard_concurrent() {
    let collector = Collector::new().unwrap().batch_size(1);
    let dropped = Arc::new(AtomicUsize::new(0));

    let objects = UnsafeSend(
        (0..cfg::THREADS)
            .map(|_| AtomicPtr::new(boxed(DropTrack(dropped.clone()))))
            .collect::<Vec<_>>(),
    );

    let guard = collector.enter_owned().unwrap();
    let barrier = Barrier::new(cfg::THREADS);

    thread::scope(|s| {
        for i in 0..cfg::THREADS {
            let guard = &guard;
            let objects = &objects;
            let dropped = &dropped;
            let barrier = &barrier;

            s.spawn(move || {
                barrier.wait();

                unsafe {
                    guard
                        .defer_retire(objects.0[i].load(Ordering::Acquire), reclaim::boxed)
                        .unwrap()
                };

                guard.flush();

                for object in objects.0.iter() {
                    let _ = unsafe { &*guard.protect(object, Ordering::Relaxed) };
                }

                assert_eq!(dropped.load(Ordering::Relaxed), 0);
            });
        }
    });

    drop(guard);
    assert_eq!(dropped.load(Ordering::Relaxed), cfg::THREADS);
}

#[test]
fn collector_equality() {
    let a = Collector::new().unwrap();
    let b = Collector::new().unwrap();

    assert_eq!(a, a);
    assert_eq!(b, b);
    assert_ne!(a, b);

    assert_eq!(*a.enter().unwrap().collector(), a);
    assert_ne!(*a.enter().unwrap().collector(), b);

    assert_eq!(*b.enter().unwrap().collector(), b);
    assert_ne!(*b.enter().unwrap().collector(), a);
}

#[test]
fn stress() {
    // stress test with operation on a shared stack
    for _ in 0..cfg::ITER {
        let stack = Arc::new(Stack::new(1));

        thread::scope(|s| {
            for i in 0..cfg::ITEMS {
                stack.push(i, &stack.collector.enter().unwrap());
                stack.pop(&stack.collector.enter().unwrap());
            }

            for _ in 0..cfg::THREADS {
                s.spawn(|| {
                    for i in 0..cfg::ITEMS {
                        stack.push(i, &stack.collector.enter().unwrap());
                        stack.pop(&stack.collector.enter().unwrap());
                    }
                });
            }
        });

        assert!(stack.pop(&stack.collector.enter().unwrap()).is_none());
        assert!(stack.is_empty());
    }
}

#[test]
fn shared_owned_stress() {
    // all threads sharing an owned guard
    for _ in 0..cfg::ITER {
        let stack = Arc::new(Stack::new(1));
        let guard = &stack.collector.enter_owned().unwrap();

        thread::scope(|s| {
            for i in 0..cfg::ITEMS {
                stack.push(i, guard);
                stack.pop(guard);
            }

            for _ in 0..cfg::THREADS {
                s.spawn(|| {
                    for i in 0..cfg::ITEMS {
                        stack.push(i, guard);
                        stack.pop(guard);
                    }
                });
            }
        });

        assert!(stack.pop(guard).is_none());
        assert!(stack.is_empty());
    }
}

#[test]
fn owned_stress() {
    // all threads creating an owned guard (this is very unrealistic and stresses
    // tls synchronization)
    for _ in 0..cfg::ITER {
        let stack = Arc::new(Stack::new(1));

        thread::scope(|s| {
            for i in 0..cfg::ITEMS {
                let guard = &stack.collector.enter_owned().unwrap();
                stack.push(i, guard);
                stack.pop(guard);
            }

            for _ in 0..cfg::THREADS {
                s.spawn(|| {
                    for i in 0..cfg::ITEMS {
                        let guard = &stack.collector.enter_owned().unwrap();
                        stack.push(i, guard);
                        stack.pop(guard);
                    }
                });
            }
        });

        assert!(stack.pop(&stack.collector.enter_owned().unwrap()).is_none());
        assert!(stack.is_empty());
    }
}

/// The state shared with a `FlakyAlloc`.
#[derive(Default)]
struct FlakyState {
    allocations: AtomicUsize,
    failures: AtomicUsize,
    live: AtomicUsize,
    failing: AtomicBool,
}

/// An allocator that rejects a fixed fraction of allocations, simulating
/// intermittent memory pressure.
///
/// Whether an allocation is rejected is decided by hashing its index, rather
/// than by sampling a random source, so the failure pattern is fixed for a
/// given sequence of allocations.
#[derive(Clone)]
struct FlakyAlloc {
    state: Arc<FlakyState>,
    fail_percent: u64,
}

impl FlakyAlloc {
    /// Create an allocator that rejects `fail_percent` percent of allocations
    /// once memory pressure is enabled.
    fn new(fail_percent: u64) -> FlakyAlloc {
        FlakyAlloc {
            state: Arc::default(),
            fail_percent,
        }
    }

    /// Set whether allocations may be rejected.
    fn set_failing(&self, failing: bool) {
        self.state.failing.store(failing, Ordering::Relaxed);
    }

    /// Returns the number of allocations that were rejected.
    fn failures(&self) -> usize {
        self.state.failures.load(Ordering::Relaxed)
    }

    /// Returns the number of bytes allocated and not yet freed.
    fn live(&self) -> usize {
        self.state.live.load(Ordering::Relaxed)
    }
}

// Safety: Allocations are either forwarded to the global allocator or rejected,
// and deallocation always succeeds.
unsafe impl Allocator for FlakyAlloc {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let index = self.state.allocations.fetch_add(1, Ordering::Relaxed) as u64;

        // Hash the index to avoid the failure pattern resonating with the
        // allocation pattern of the collector.
        if self.state.failing.load(Ordering::Relaxed)
            && (index.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 33) % 100 < self.fail_percent
        {
            self.state.failures.fetch_add(1, Ordering::Relaxed);
            return Err(AllocError);
        }

        let ptr = Global.allocate(layout)?;
        self.state.live.fetch_add(layout.size(), Ordering::Relaxed);
        Ok(ptr)
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        self.state.live.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { Global.deallocate(ptr, layout) }
    }
}

/// The state shared with a `BudgetAlloc`.
#[derive(Default)]
struct BudgetState {
    live: AtomicUsize,
    budget: AtomicUsize,
}

/// An allocator with a fixed memory budget, rejecting any allocation that would
/// exceed it.
#[derive(Clone, Default)]
struct BudgetAlloc(Arc<BudgetState>);

impl BudgetAlloc {
    /// Create an allocator that can allocate at most `budget` bytes at a time.
    fn new(budget: usize) -> BudgetAlloc {
        let alloc = BudgetAlloc::default();
        alloc.set_budget(budget);
        alloc
    }

    /// Set the number of bytes this allocator may keep live.
    fn set_budget(&self, budget: usize) {
        self.0.budget.store(budget, Ordering::Relaxed);
    }

    /// Returns the number of bytes allocated and not yet freed.
    fn live(&self) -> usize {
        self.0.live.load(Ordering::Relaxed)
    }
}

// Safety: Allocations are either forwarded to the global allocator or rejected,
// and deallocation always succeeds.
unsafe impl Allocator for BudgetAlloc {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        // Claim the memory before allocating it, ensuring that concurrent
        // allocations can never exceed the budget between the check and the
        // allocation.
        self.0
            .live
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |live| {
                let live = live.checked_add(layout.size())?;
                (live <= self.0.budget.load(Ordering::Relaxed)).then_some(live)
            })
            .map_err(|_| AllocError)?;

        match Global.allocate(layout) {
            Ok(ptr) => Ok(ptr),
            Err(err) => {
                // Release the claim if the underlying allocation failed.
                self.0.live.fetch_sub(layout.size(), Ordering::Release);
                Err(err)
            }
        }
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        self.0.live.fetch_sub(layout.size(), Ordering::Release);
        unsafe { Global.deallocate(ptr, layout) }
    }
}

/// The outcome of retiring values under memory pressure.
#[derive(Default)]
struct Pressure {
    /// The number of values that were created.
    created: AtomicUsize,

    /// The number of operations that failed to allocate.
    rejected: AtomicUsize,
}

/// Retire `items` values through guards created by `enter`, tolerating
/// allocation failure at every step.
///
/// Values that could not be retired are freed directly, which is sound because
/// they were never shared with another thread. Note that a value is only
/// created if a guard could be entered, so the number of values created is
/// recorded rather than assumed.
fn retire_under_pressure<G>(
    items: usize,
    dropped: &Arc<AtomicUsize>,
    pressure: &Pressure,
    mut enter: impl FnMut() -> Result<G, AllocError>,
) where
    G: Guard,
{
    for _ in 0..items {
        // Entering the collector allocates thread-local storage the first time
        // it is called on a thread, and so can fail under memory pressure.
        let Ok(guard) = enter() else {
            pressure.rejected.fetch_add(1, Ordering::Relaxed);
            continue;
        };

        let value = boxed(DropTrack(dropped.clone()));
        pressure.created.fetch_add(1, Ordering::Relaxed);

        // Safety: The value was never shared, and was allocated with `Box`.
        if unsafe { guard.defer_retire(value, reclaim::boxed) }.is_err() {
            // The failed retirement left the value untouched, so we still own
            // it, and can free it directly.
            //
            // Safety: The retirement failed, so the value was never retired.
            drop(unsafe { Box::from_raw(value) });
            pressure.rejected.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[test]
fn flaky_allocator_stress() {
    let alloc = FlakyAlloc::new(50);
    let collector = Collector::new_in(alloc.clone()).unwrap().batch_size(8);
    let dropped = Arc::new(AtomicUsize::new(0));
    let pressure = Pressure::default();

    // Simulate memory pressure only once the collector has been created.
    alloc.set_failing(true);

    thread::scope(|s| {
        // Half of the threads use local guards.
        for _ in 0..cfg::THREADS / 2 {
            s.spawn(|| {
                retire_under_pressure(cfg::ITEMS, &dropped, &pressure, || collector.enter());
            });
        }

        // The other half use owned guards, which allocate independently of the
        // current thread.
        for _ in 0..cfg::THREADS / 2 {
            s.spawn(|| {
                retire_under_pressure(cfg::ITEMS, &dropped, &pressure, || collector.enter_owned());
            });
        }
    });

    // Memory pressure was actually simulated.
    assert!(alloc.failures() > 0);
    assert!(pressure.rejected.load(Ordering::Relaxed) > 0);

    // Dropping the collector reclaims everything that was retired. Note that
    // reclamation never allocates, so it succeeds despite the pressure.
    drop(collector);

    // Every value was either reclaimed or freed after a failed retirement, and
    // none of them were reclaimed twice.
    assert_eq!(
        dropped.load(Ordering::Relaxed),
        pressure.created.load(Ordering::Relaxed)
    );

    // The collector freed everything it allocated, despite the failures.
    assert_eq!(alloc.live(), 0);
}

#[test]
fn budget_allocator_stress() {
    let alloc = BudgetAlloc::new(usize::MAX);
    let collector = Collector::new_in(alloc.clone()).unwrap().batch_size(8);
    let dropped = Arc::new(AtomicUsize::new(0));
    let pressure = Pressure::default();

    // Allow the collector very little memory beyond what it has already
    // allocated, so threads contend for the remaining budget.
    alloc.set_budget(alloc.live() + 512);

    thread::scope(|s| {
        for _ in 0..cfg::THREADS {
            s.spawn(|| {
                retire_under_pressure(cfg::ITEMS, &dropped, &pressure, || collector.enter());
            });
        }
    });

    // The budget was actually exhausted.
    assert!(pressure.rejected.load(Ordering::Relaxed) > 0);

    // Everything is reclaimed once the collector is dropped.
    drop(collector);

    assert_eq!(
        dropped.load(Ordering::Relaxed),
        pressure.created.load(Ordering::Relaxed)
    );
    assert_eq!(alloc.live(), 0);
}

#[test]
fn budget_exhaustion() {
    // A collector cannot be created without enough memory for its thread-local
    // storage.
    assert!(Collector::new_in(BudgetAlloc::new(0)).is_err());

    let alloc = BudgetAlloc::new(usize::MAX);
    let collector = Collector::new_in(alloc.clone()).unwrap().batch_size(4);
    let dropped = Arc::new(AtomicUsize::new(0));

    let guard = collector.enter().unwrap();

    // Cap the budget at what the collector has already allocated, exhausting it.
    alloc.set_budget(alloc.live());

    // Retiring reports the failure instead of aborting.
    let value = boxed(DropTrack(dropped.clone()));

    // Safety: The value was never shared, and was allocated with `Box`.
    assert!(unsafe { guard.defer_retire(value, reclaim::boxed) }.is_err());
    assert!(guard.reserve_retire().is_err());

    // The failed retirement left the value untouched, so we still own it.
    assert_eq!(dropped.load(Ordering::Relaxed), 0);

    // Safety: The retirement failed, so the value was never retired.
    drop(unsafe { Box::from_raw(value) });
    assert_eq!(dropped.load(Ordering::Relaxed), 1);

    // Nor does the collector abort on a thread it has never seen before, which
    // has neither thread-local storage nor a retirement batch. Note that which
    // of the two allocations the thread needs fails first depends on how much
    // memory the collector happened to reserve, so the thread reports whether
    // it got far enough to create a value.
    let created = thread::scope(|s| {
        s.spawn(|| {
            let Ok(guard) = collector.enter() else {
                return false;
            };

            let value = boxed(DropTrack(dropped.clone()));

            // Safety: The value was never shared, and was allocated with `Box`.
            assert!(unsafe { guard.defer_retire(value, reclaim::boxed) }.is_err());

            // Safety: The retirement failed, so the value was never retired.
            drop(unsafe { Box::from_raw(value) });

            true
        })
        .join()
        .unwrap()
    });

    let dropped_so_far = 1 + created as usize;
    assert_eq!(dropped.load(Ordering::Relaxed), dropped_so_far);

    // Retirement succeeds again once memory is available.
    alloc.set_budget(usize::MAX);
    let value = boxed(DropTrack(dropped.clone()));

    // Safety: The value was never shared, and was allocated with `Box`.
    unsafe { guard.defer_retire(value, reclaim::boxed) }.unwrap();

    drop(guard);
    drop(collector);

    assert_eq!(dropped.load(Ordering::Relaxed), dropped_so_far + 1);
    assert_eq!(alloc.live(), 0);
}

/// A stack that never aborts, and never leaks, under memory pressure.
///
/// Note that only the collector allocates through the flaky allocator; nodes
/// are allocated with `Box` to keep the test focused on the collector.
struct PressureStack {
    head: AtomicPtr<Node<DropTrack>>,
    collector: Collector,

    /// Nodes that could not be retired, freed once the stack is quiesced.
    stashed: Mutex<Vec<UnsafeSend<*mut Node<DropTrack>>>>,
}

impl PressureStack {
    fn new(alloc: FlakyAlloc, batch_size: usize) -> PressureStack {
        PressureStack {
            head: AtomicPtr::new(ptr::null_mut()),
            collector: Collector::new_in(alloc).unwrap().batch_size(batch_size),
            stashed: Mutex::new(Vec::new()),
        }
    }

    /// Push a value onto the stack.
    ///
    /// Note that pushing never allocates through the collector.
    fn push(&self, value: DropTrack, guard: &impl Guard) {
        let new = boxed(Node {
            data: ManuallyDrop::new(value),
            next: ptr::null_mut(),
        });

        loop {
            let head = guard.protect(&self.head, Ordering::Relaxed);
            unsafe { (*new).next = head }

            if self
                .head
                .compare_exchange(head, new, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
        }
    }

    /// Pop a value off of the stack.
    fn pop(&self, guard: &impl Guard) -> Option<DropTrack> {
        loop {
            let head = guard.protect(&self.head, Ordering::Acquire);

            if head.is_null() {
                return None;
            }

            let next = unsafe { (*head).next };

            // Reserve the retirement before unlinking the node. Once the node is
            // unlinked it is unreachable to new readers and *must* be retired,
            // a position from which an allocation failure could not be handled.
            let reserved = guard.reserve_retire().is_ok();

            if self
                .head
                .compare_exchange(head, next, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                // Safety: We unlinked the node, so we have unique ownership of
                // its data.
                let data = unsafe { ptr::read(&(*head).data) };

                // Safety: The node is unreachable to new readers, and was
                // allocated with `Box`.
                let retired = unsafe { guard.defer_retire(head, reclaim::boxed) };

                if retired.is_err() {
                    // The reservation should have made this infallible.
                    assert!(!reserved);

                    // The node cannot be freed here, as concurrent readers may
                    // still hold a reference to it, so it is stashed and freed
                    // once every thread has finished.
                    self.stashed.lock().unwrap().push(UnsafeSend(head));
                }

                return Some(ManuallyDrop::into_inner(data));
            }
        }
    }

    /// Free the nodes that could not be retired, returning their number.
    ///
    /// # Safety
    ///
    /// No thread may be accessing the stack, and all guards must have been
    /// dropped.
    unsafe fn free_stashed(&self) -> usize {
        let mut stashed = self.stashed.lock().unwrap();
        let count = stashed.len();

        for node in stashed.drain(..) {
            // Safety: The caller guarantees that no thread can be accessing the
            // node, and it was allocated with `Box`.
            drop(unsafe { Box::from_raw(node.0) });
        }

        count
    }
}

#[test]
fn pressure_stack_stress() {
    let alloc = FlakyAlloc::new(25);
    let stack = PressureStack::new(alloc.clone(), 4);
    let dropped = Arc::new(AtomicUsize::new(0));
    let pushed = AtomicUsize::new(0);

    // Simulate memory pressure only once the stack has been created.
    alloc.set_failing(true);

    thread::scope(|s| {
        for _ in 0..cfg::THREADS {
            s.spawn(|| {
                for _ in 0..cfg::ITEMS {
                    // Entering can fail under pressure, in which case there is
                    // nothing to do but try again later.
                    let Ok(guard) = stack.collector.enter() else {
                        continue;
                    };

                    stack.push(DropTrack(dropped.clone()), &guard);
                    pushed.fetch_add(1, Ordering::Relaxed);
                    stack.pop(&guard);
                }
            });
        }
    });

    // Memory pressure was actually simulated.
    assert!(alloc.failures() > 0);

    // Drain whatever is left on the stack once the pressure subsides.
    alloc.set_failing(false);
    let guard = stack.collector.enter().unwrap();
    while stack.pop(&guard).is_some() {}
    drop(guard);

    assert!(stack.head.load(Ordering::Relaxed).is_null());

    // Every value that was pushed was popped, and dropped exactly once.
    assert_eq!(
        dropped.load(Ordering::Relaxed),
        pushed.load(Ordering::Relaxed)
    );

    // Safety: All threads have finished and all guards have been dropped, so
    // the stashed nodes are unreachable.
    unsafe { stack.free_stashed() };

    drop(stack.collector);
    assert_eq!(alloc.live(), 0);
}

#[derive(Debug)]
pub struct Stack<T> {
    head: AtomicPtr<Node<T>>,
    collector: Collector,
}

#[derive(Debug)]
struct Node<T> {
    data: ManuallyDrop<T>,
    next: *mut Node<T>,
}

impl<T> Stack<T> {
    pub fn new(batch_size: usize) -> Stack<T> {
        Stack {
            head: AtomicPtr::new(ptr::null_mut()),
            collector: Collector::new().unwrap().batch_size(batch_size),
        }
    }

    pub fn push(&self, value: T, guard: &impl Guard) {
        let new = boxed(Node {
            data: ManuallyDrop::new(value),
            next: ptr::null_mut(),
        });

        loop {
            let head = guard.protect(&self.head, Ordering::Relaxed);
            unsafe { (*new).next = head }

            if self
                .head
                .compare_exchange(head, new, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
        }
    }

    pub fn pop(&self, guard: &impl Guard) -> Option<T> {
        loop {
            let head = guard.protect(&self.head, Ordering::Acquire);

            if head.is_null() {
                return None;
            }

            let next = unsafe { (*head).next };

            if self
                .head
                .compare_exchange(head, next, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                unsafe {
                    let data = ptr::read(&(*head).data);
                    self.collector.retire(head, reclaim::boxed).unwrap();
                    return Some(ManuallyDrop::into_inner(data));
                }
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.head.load(Ordering::Relaxed).is_null()
    }
}

impl<T> Drop for Stack<T> {
    fn drop(&mut self) {
        let guard = self.collector.enter().unwrap();
        while self.pop(&guard).is_some() {}
    }
}

#[cfg(any(miri, seize_asan))]
mod cfg {
    pub const THREADS: usize = 4;
    pub const ITEMS: usize = 100;
    pub const ITER: usize = 4;
}

#[cfg(not(any(miri, seize_asan)))]
mod cfg {
    pub const THREADS: usize = 32;
    pub const ITEMS: usize = 10_000;
    pub const ITER: usize = 50;
}
