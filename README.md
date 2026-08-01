# `seize`

[<img alt="crates.io" src="https://img.shields.io/crates/v/seize?style=for-the-badge" height="25">](https://crates.io/crates/seize)
[<img alt="github" src="https://img.shields.io/badge/github-seize-blue?style=for-the-badge" height="25">](https://github.com/ibraheemdev/seize)
[<img alt="docs.rs" src="https://img.shields.io/docsrs/seize?style=for-the-badge" height="25">](https://docs.rs/seize)

Fast, efficient, and predictable memory reclamation for concurrent data
structures.

Refer to the [quick-start guide] to get started.

## Background

Concurrent data structures are faced with the problem of deciding when it is
safe to free memory. Despite an object being logically removed, it may still be
accessible by other threads that are holding references to it, and thus it is
not safe to free immediately. Over the years, many algorithms have been devised
to solve this problem. However, most traditional memory reclamation schemes make
a tradeoff between performance and efficiency.

For example, [hazard pointers] track individual pointers, making them very
memory efficient but also relatively slow. On the other hand, [epoch based
reclamation] is fast and lightweight, but lacks predictability, requiring
periodic checks to determine when it is safe to free memory. This can cause
reclamation to trigger unpredictably, leading to poor latency distributions.

Alternative epoch-based schemes forgo workload balancing, relying on the thread
that retires an object always being the one that frees it. While this can avoid
synchronization costs, it also leads to unbalanced reclamation in read-dominated
workloads; parallelism is reduced when only a fraction of threads are writing,
degrading memory efficiency as well as performance.

## Implementation

`seize` is based on the [hyaline reclamation scheme], which uses reference
counting to determine when it is safe to free memory. However, unlike
traditional reference counting schemes where every memory access requires
modifying shared memory, reference counters are only used for retired objects.
When a batch of objects is retired, a reference counter is initialized and
propagated to all active threads. Threads cooperate to decrement the reference
counter as they exit, eventually freeing the batch. Reclamation is naturally
balanced as the thread with the last reference to an object is the one that
frees it. This also removes the need to check whether other threads have made
progress, leading to predictable latency without sacrificing performance.

`seize` provides performance competitive with that of epoch based schemes, while
memory efficiency is similar to that of hazard pointers. `seize` is compatible
with all modern hardware that supports single-word atomic operations such as FAA
and CAS.

## Allocation

`seize` never aborts on allocation failure. Its internal allocations go through
the [`Allocator`] trait, defaulting to the global allocator, and every operation
that may allocate returns a `Result`, leaving the collector unchanged on
failure. A custom allocator can be provided with `Collector::new_in`.

Two caveats. If a thread exits while allocation is failing, its thread ID may
not be recycled, leaving thread-local storage sparser than it would otherwise
be; this resolves as soon as memory is available again. And `seize` still
panics on thread-ID exhaustion (after `2^64` thread creations) and on lock
poisoning.

## No-std

`seize` builds without an operating system, as `core` + `alloc`, by disabling
the default `std` feature. This is for a freestanding target that is still
genuinely concurrent — several cores, or an RTOS with preemptive tasks — not
for removing the cost of concurrency on a single-threaded one.

What changes is where a thread slot comes from. With `std`, `Collector::enter`
looks one up in thread-local storage. Without it there is no thread-local
storage, so the slot is owned explicitly: `Collector::enter_owned` allocates
one and frees it when the guard is dropped. The guard is `Send`, and is meant
to live in whatever handle the host uses to represent a worker. Everything
gated on `std` — `LocalGuard`, `Collector::enter`, and the two
current-thread retirement methods `Collector::retire` and
`Collector::reserve_retire` — has an `OwnedGuard` equivalent.

Locking becomes the host's job. `seize` ships no lock protocol of its own
without `std`: its two internal locks are delegated to whatever the host
registers through [`seize::lock`] — an `acquire`/`release` pair over an
opaque key, registered once at startup. A Zig host hands over a
`std.Thread.Mutex` in four lines; an RTOS hands over its native mutex and
gets priority inheritance for free. If nothing is registered by the time the
first lock is taken, `seize` latches onto a built-in test-and-set spin lock,
which is correct but can deadlock a strictly priority-scheduled single core;
the locks are only taken when a thread slot is allocated or freed and when an
`OwnedGuard` is mutated, so one guard per worker up front never contends them.
And the collector sizes its initial thread-local storage from
`available_parallelism`, which does not exist here, so it starts from one and
grows on demand — pass a `batch_size` if the default of `32` does not suit the
part.

The target floor is real atomic compare-and-swap: `thumbv7em`,
`thumbv8m.main`, `riscv32imac` and up. `thumbv6m` (Cortex-M0/M0+) has no CAS
and is not supported. Nothing in the crate uses 64-bit atomics, so 32-bit
targets are fine. Fast memory barriers are a Linux and Windows optimization,
so `fast-barrier` implies `std`; without it, heavy barriers are a plain
`SeqCst` fence, which costs performance rather than correctness.

[quick-start guide]: https://docs.rs/seize/latest/seize/guide/index.html
[hazard pointers]:
  https://www.cs.otago.ac.nz/cosc440/readings/hazard-pointers.pdf
[`Allocator`]: https://docs.rs/seize/latest/seize/alloc/trait.Allocator.html
[`seize::lock`]: https://docs.rs/seize/latest/seize/lock/index.html
[hyaline reclamation scheme]: https://arxiv.org/pdf/1905.07903.pdf
[epoch based reclamation]:
  https://www.cl.cam.ac.uk/techreports/UCAM-CL-TR-579.pdf
