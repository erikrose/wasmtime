//! Scheduling of MMU-based interruption: handing out interrupt pages to stores
//! and protecting them when it's time to interrupt.

use crate::prelude::*;
use core::ffi::c_void;
use core::ptr::NonNull;

mod wheel;
pub use wheel::TimerWheelInterrupter;

/// A reference to an MMU interrupt page. It is intended that an
/// `MmuInterrupter` may need to squirrel away opaque data herein.
pub trait PageHandle: Send + Sync {
    /// Returns the interrupt page pointer: the memory address to attempt to
    /// load at checkpoints.
    fn page_ptr(&self) -> NonNull<c_void>;
}

/// Hands out interrupt pages to stores and protects them (rendering them
/// unreadable) when those stores are due to be interrupted.
pub trait MmuInterrupter: Send + Sync {
    /// Fetches an unprotected interrupt page. A scheduling mechanism must
    /// protect it (rendering it unreadable) at an appropriate time in the
    /// future to effect interruption.
    fn acquire_page(&self) -> Box<dyn PageHandle>;

    /// Renounces a store's claim on an interrupt page, declaring that the store
    /// no longer interrupts if the page becomes unreadable. It is a logic error
    /// to release a page and not immediately acquire a new one when the
    /// corresponding store has any fibers in the Executing state.
    ///
    /// Implementations must keep released pages mapped while any Engine using
    /// them might yet run any Wasm: compiled code may load from a stale pointer
    /// to one, and only an access fault (not an unmapped-address fault) is
    /// caught as an interruption.
    fn release_page(&self, page: Box<dyn PageHandle>);
}
