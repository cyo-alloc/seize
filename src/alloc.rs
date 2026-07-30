//! Configurable memory allocation.
//!
//! Memory that a [`Collector`](crate::Collector) allocates internally — its
//! thread-local storage and its batches of retired objects — is allocated
//! through the [`Allocator`] trait, re-exported here from
//! [`allocator_api2`].
//!
//! By default a collector allocates in the global allocator. A different
//! allocator can be provided per-collector with
//! [`Collector::new_in`](crate::Collector::new_in).
//!
//! Note that this only affects the collector's own allocations. Objects that
//! are retired through a collector are freed by the reclaimer they were
//! retired with, and are never allocated by seize itself.
//!
//! # Allocation Failure
//!
//! Seize never aborts on allocation failure. Every operation that may allocate
//! returns a [`Result`], and leaves the collector unchanged if allocation
//! fails.

use std::fmt;
use std::ptr::NonNull;
use std::sync::Arc;

pub use allocator_api2::alloc::{AllocError, Allocator, Global, Layout};

/// A type-erased allocator.
///
/// Erasing the allocator keeps it out of the signatures of every type that can
/// reach a collector, such as guards and retired entries, which only need to
/// *reach* the allocator rather than name it. The `Global` variant keeps the
/// default case a static call, avoiding both dynamic dispatch and the reference
/// count.
#[derive(Clone)]
pub(crate) enum DynAlloc {
    /// The global allocator.
    Global,

    /// A user-provided allocator.
    Dyn(Arc<dyn Allocator + Send + Sync>),
}

impl DynAlloc {
    /// Erase the given allocator.
    #[inline]
    pub fn new<A>(alloc: A) -> DynAlloc
    where
        A: Allocator + Send + Sync + 'static,
    {
        DynAlloc::Dyn(Arc::new(alloc))
    }

    /// Returns a reference to the underlying allocator.
    #[inline]
    pub fn as_dyn(&self) -> &(dyn Allocator + '_) {
        match self {
            DynAlloc::Global => &Global,
            DynAlloc::Dyn(alloc) => &**alloc,
        }
    }
}

impl fmt::Debug for DynAlloc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DynAlloc::Global => f.write_str("Global"),
            DynAlloc::Dyn(_) => f.debug_struct("Allocator").finish_non_exhaustive(),
        }
    }
}

// Safety: All methods forward to an allocator that upholds the `Allocator`
// contract. Note that every method is forwarded, rather than relying on the
// default implementations, to preserve any optimizations of the underlying
// allocator.
unsafe impl Allocator for DynAlloc {
    #[inline]
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        match self {
            DynAlloc::Global => Global.allocate(layout),
            DynAlloc::Dyn(alloc) => alloc.allocate(layout),
        }
    }

    #[inline]
    fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        match self {
            DynAlloc::Global => Global.allocate_zeroed(layout),
            DynAlloc::Dyn(alloc) => alloc.allocate_zeroed(layout),
        }
    }

    #[inline]
    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        // Safety: Guaranteed by caller.
        unsafe {
            match self {
                DynAlloc::Global => Global.deallocate(ptr, layout),
                DynAlloc::Dyn(alloc) => alloc.deallocate(ptr, layout),
            }
        }
    }

    #[inline]
    unsafe fn grow(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        // Safety: Guaranteed by caller.
        unsafe {
            match self {
                DynAlloc::Global => Global.grow(ptr, old_layout, new_layout),
                DynAlloc::Dyn(alloc) => alloc.grow(ptr, old_layout, new_layout),
            }
        }
    }

    #[inline]
    unsafe fn grow_zeroed(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        // Safety: Guaranteed by caller.
        unsafe {
            match self {
                DynAlloc::Global => Global.grow_zeroed(ptr, old_layout, new_layout),
                DynAlloc::Dyn(alloc) => alloc.grow_zeroed(ptr, old_layout, new_layout),
            }
        }
    }

    #[inline]
    unsafe fn shrink(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        // Safety: Guaranteed by caller.
        unsafe {
            match self {
                DynAlloc::Global => Global.shrink(ptr, old_layout, new_layout),
                DynAlloc::Dyn(alloc) => alloc.shrink(ptr, old_layout, new_layout),
            }
        }
    }
}

/// A `Box` allocated in a collector's allocator.
pub(crate) type Box<T> = allocator_api2::boxed::Box<T, DynAlloc>;

/// A `Vec` allocated in a collector's allocator.
pub(crate) type Vec<T> = allocator_api2::vec::Vec<T, DynAlloc>;

/// Allocates a block of memory fitting the given layout.
///
/// Note that seize never aborts on allocation failure; errors are propagated to
/// the caller.
pub(crate) fn allocate(alloc: &DynAlloc, layout: Layout) -> Result<NonNull<u8>, AllocError> {
    alloc.allocate(layout).map(NonNull::cast::<u8>)
}

/// Deallocates a block of memory allocated by [`allocate`].
///
/// # Safety
///
/// `ptr` and `layout` must describe a live block previously returned by
/// [`allocate`] with the same allocator.
pub(crate) unsafe fn deallocate(alloc: &DynAlloc, ptr: NonNull<u8>, layout: Layout) {
    // Safety: Guaranteed by caller.
    unsafe { alloc.deallocate(ptr, layout) }
}
