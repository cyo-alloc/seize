#![allow(clippy::missing_transmute_annotations)]
#![deny(unsafe_op_in_unsafe_fn)]
#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc = include_str!("../README.md")]

extern crate alloc as alloc_crate;

#[cfg(feature = "std")]
extern crate std;

// The unit tests for the `no_std` locks use threads from the test harness.
#[cfg(all(test, not(feature = "std")))]
extern crate std;

mod collector;
mod guard;
mod raw;
mod sync;

pub mod alloc;
pub mod guide;
pub mod lock;
pub mod reclaim;

pub use collector::Collector;
pub use guard::{Guard, OwnedGuard};

#[cfg(feature = "std")]
#[cfg_attr(docsrs, doc(cfg(feature = "std")))]
pub use guard::LocalGuard;
