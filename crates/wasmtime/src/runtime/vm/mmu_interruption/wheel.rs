//! An [`MmuInterrupter`] implemented as a timer wheel

use super::{MmuInterrupter, PageHandle};
use crate::prelude::*;
use crate::runtime::vm::SendSyncPtr;
use core::ffi::c_void;
use core::ptr::{self, NonNull};
use core::time::Duration;
use rustix::mm::{MapFlags, ProtFlags, mmap_anonymous};
use rustix::param::page_size;
use std::sync::Mutex;

/// A timer-driven [`MmuInterrupter`] whose ticker thread is started and
/// stopped by the embedder
///
/// This is currently a placeholder: it hands out pages but never protects
/// them, so Wasm running under it is never interrupted.
pub struct TimerWheelInterrupter {
    timeslice: Duration,
    resolution: Duration,
    /// Released pages, kept mapped (as `MmuInterrupter::release_page` requires)
    /// for reuse.
    free_pages: Mutex<Vec<SendSyncPtr<c_void>>>,
}

impl TimerWheelInterrupter {
    /// The uninterrupted period given to each Store, unless overridden by
    /// [`TimerWheelInterruper::with_timeslice`].
    pub const DEFAULT_TIMESLICE: Duration = Duration::from_millis(5);

    /// The resolution used unless overridden by
    /// [`TimerWheelInterruper::with_resolution`].
    pub const DEFAULT_RESOLUTION: Duration = Duration::from_millis(1);

    /// Creates an unstarted interrupter with the default timeslice and
    /// resolution.
    pub fn new() -> Self {
        Self {
            timeslice: Self::DEFAULT_TIMESLICE,
            resolution: Self::DEFAULT_RESOLUTION,
            free_pages: Mutex::default(),
        }
    }

    /// Sets how long a store may run before it is interrupted: at the first
    /// slot boundary at least `timeslice` after it starts running.
    pub fn with_timeslice(mut self, timeslice: Duration) -> Self {
        self.timeslice = timeslice;
        self
    }

    /// Sets the time duration of each slot on the wheel.
    pub fn with_resolution(mut self, resolution: Duration) -> Self {
        self.resolution = resolution;
        self
    }

    /// Starts the ticker thread. Until then, nothing is interrupted.
    pub fn start(&self) {}

    /// Stops and joins the ticker thread, if running.
    pub fn stop(&self) {}
}

impl Default for TimerWheelInterrupter {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TimerWheelInterrupter {
    fn drop(&mut self) {
        self.stop();
    }
}

struct TimerWheelPageHandle(SendSyncPtr<c_void>);

impl PageHandle for TimerWheelPageHandle {
    fn page_ptr(&self) -> NonNull<c_void> {
        self.0.as_non_null()
    }
}

impl MmuInterrupter for TimerWheelInterrupter {
    fn acquire_page(&self) -> Box<dyn PageHandle> {
        let reused = self.free_pages.lock().unwrap().pop();
        Box::new(TimerWheelPageHandle(reused.unwrap_or_else(map_page)))
    }

    fn release_page(&self, page: Box<dyn PageHandle>) {
        self.free_pages
            .lock()
            .unwrap()
            .push(SendSyncPtr::new(page.page_ptr()));
    }
}

/// Maps a fresh, readable page, which is never unmapped.
fn map_page() -> SendSyncPtr<c_void> {
    // SAFETY: Passing a null address lets the kernel choose where to map, so
    // no existing mapping can be clobbered.
    let page = unsafe {
        mmap_anonymous(
            ptr::null_mut(),
            page_size(),
            ProtFlags::READ,
            MapFlags::PRIVATE,
        )
    }
    .expect("an interrupt page should be allocable");
    SendSyncPtr::new(NonNull::new(page).expect("a successful mmap should not return null"))
}
