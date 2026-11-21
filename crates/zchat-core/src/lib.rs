#![no_std]
#![forbid(unsafe_code)]
//! Bounded protocol values, requests, permissions and resumable messaging events.
//!
//! Platform-specific parameters remain intact. The shared operation list describes
//! the host contract; each plugin must separately advertise what it implements.

extern crate alloc;
#[cfg(any(test, feature = "std"))]
extern crate std;

mod error;
mod journal;
pub mod json;
mod request;

pub use error::{ChatError, Result};
pub use journal::{AppendOutcome, Event, EventJournal, EventPage};
pub use json::{JsonLimits, Value, arr, obj, parse, parse_with_limits};
pub use request::{
    Actor, DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE, OPERATIONS, Operation, Request, RequestPolicy,
    catalog, operation, operation_info, operations_manifest,
};
