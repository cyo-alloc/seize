use seize::alloc::{AllocError, Allocator, Global, Layout};
use seize::{reclaim, Collector, Guard};

use std::mem::ManuallyDrop;
use std::ptr;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Barrier};
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
        collector.retire(ptr, |ptr: *mut Recursive, collector| {
            let value = Box::from_raw(ptr);

            for pointer in value.pointers {
                collector.retire(pointer, reclaim::boxed).unwrap();

                let mut guard = collector.enter().unwrap();
                guard.flush();
                guard.refresh();
                drop(guard);
            }
        }).unwrap();

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
            unsafe { collector.retire(item.load(Ordering::Relaxed), reclaim::boxed).unwrap() };
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

        collector.retire(ptr, |ptr: *mut Recursive, collector| {
            let value = Box::from_raw(ptr);
            for pointer in value.pointers {
                (*collector).retire(pointer, reclaim::boxed).unwrap();
            }
        }).unwrap();

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
            unsafe { guard2.defer_retire(object.load(Ordering::Acquire), reclaim::boxed).unwrap() }
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

                unsafe { guard.defer_retire(objects.0[i].load(Ordering::Acquire), reclaim::boxed).unwrap() };

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
