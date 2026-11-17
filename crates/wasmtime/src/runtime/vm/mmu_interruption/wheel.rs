//! An [`MmuInterrupter`] implemented as a timing wheel

use super::MmuInterrupter;
use crate::prelude::*;
use crate::runtime::vm::mmap::AlignedLength;
use crate::runtime::vm::{Mmap, host_page_size};
use core::ffi::c_void;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Weak};

/// A timing wheel that functions as an [`MmuInterrupter`]
///
/// It is driven by the embedder calling [`TimingWheelInterrupter::tick`]. Thus,
/// any relationship with wall-clock time is established by the embedder.
///
/// Stores that start running between the same two ticks share an interrupt
/// page, so one `mprotect` interrupts all of them. Thundering-herd concerns are
/// mitigated because actual interruptions are smeared out over time as
/// Cranelift-generated code checks for them. Additionally, the herds are small,
/// since they consist of running fibers only, which are bounded by the number
/// of cores. Herd size would average out to be ≤ cores / slots.len().
pub struct TimingWheelInterrupter {
    /// Number of ticks so far. Wraps in the extreme case. This points to the
    /// slot that has most recently been reached.
    ///
    /// "Reached" means "had its interrupt page set aside so no further stores
    /// can acquire it". A reached slot protects its former page immediately
    /// after setting it aside.
    ///
    /// `tick()` is the exclusive writer of this.
    hand: AtomicU64,
    /// "Hours" on the timing wheel. Each slot contains one page, which gets
    /// protected when the hand reaches the slot. Until then, the page,
    /// unprotected, is doled out to stores which ask to have an interrupt
    /// scheduled at that slot.
    slots: Box<[Mutex<Arc<Page>>]>,
    /// Where pages go once their refcounts hit zero. `tick()` then moves them
    /// to a `quarantine` bucket. Having this interstitial area saves the wheel
    /// referencing itself.
    to_quarantine: Arc<PagesToQuarantine>,
    /// State used exclusively by `tick()`
    tick_state: Mutex<TickState>,
}

struct TickState {
    /// Buckets of pages the hand has reached and whose last holding store has
    /// let go. These remain protected for `ticks_of_quarantine` ticks before
    /// becoming eligible for reuse in a slot. This narrows the chance of a
    /// stale interrupt-page-ptr cache allowing a store to run longer than it
    /// deserves.
    ///
    /// To run longer than deserved, a store would need all of these to happen:
    ///
    /// 1. Its active frame holds a stale ptr to a released page.
    /// 2. It runs no interruption check against that address for the entire
    ///    time the page spends awaiting release, in quarantine, and in `free`.
    /// 3. That very page, out of all of `free`, is the one recycled into a
    ///    slot.
    /// 4. The store resumes while the page sits in that slot, the only time it
    ///    is unprotected.
    ///
    /// This confluence of events is improbable. Even then, the overrun is
    /// bounded:
    ///
    /// 1. The stack is finite.
    /// 2. No new frames with stale caches can spring into existence during a
    ///    revolution of the wheel (that is, within `slots.len()` ticks).
    /// 3. A stale frame's page (in fact, every page) is protected within 1
    ///    revolution of the wheel.
    ///
    /// Thus, the max undeserved, uninterrupted runtime a store could achieve is
    /// `slots.len()` ticks * number stale frames on the stack. For each frame,
    /// the improbability is multiplied.
    quarantines: Box<[Vec<Arc<Page>>]>,
    /// Protected pages with no stores holding them anymore and which have aged
    /// out of `quarantines`. These are ready to be unprotected and used in
    /// `slots` as necessary. The FIFO order isn't necessary to provide our
    /// guarantees, but it effectively extends the quarantine period for free.
    /// `tick()` is the only reader or writer of this.
    free: VecDeque<Arc<Page>>,
}

impl TickState {
    fn new(ticks_of_quarantine: usize) -> Self {
        Self {
            quarantines: (0..ticks_of_quarantine).map(|_| Vec::new()).collect(),
            free: Default::default(),
        }
    }
}

/// Returns an index into a `len`-length vector corresponding to `hand`, modding
/// as necessary to fit.
fn wraparound(hand: u64, len: usize) -> usize {
    let len = u64::try_from(len).unwrap();
    usize::try_from(hand % len).unwrap()
}

impl TimingWheelInterrupter {
    /// Returns an interrupter that gives each store at least
    /// `ticks_per_interrupt` full tick intervals (and at most
    /// `ticks_per_interrupt` + 1) before interrupting it. `ticks_of_quarantine`
    /// controls how long an interrupt page is kept protected and out of service
    /// before being recycled into a wheel slot, reducing the chance for a store
    /// to run longer than deserved due to stale interrupt page ptr caches. Good
    /// default choices for this are 1-2x `ticks_per_interrupt`. It must be at
    /// least 1.
    pub fn new(ticks_per_interrupt: u32, ticks_of_quarantine: u32) -> Self {
        // The +1 makes these "at least N ticks". (Without, it would mean "at
        // most N ticks".) The +1 compensates for the fraction of the current
        // tick that passed before a store acquired its page.
        let slots = usize::try_from(ticks_per_interrupt).unwrap() + 1;
        // We don't need a +1 on this, as `to_quarantine` doesn't get emptied
        // into `quarantines` until the next tick.
        let ticks_of_quarantine = usize::try_from(ticks_of_quarantine).unwrap();
        // Aside from being a good idea, at least 1 tick of quarantine
        // simplifies the tick code from having to branch around div-by-zeroes.
        assert_ne!(
            ticks_of_quarantine, 0,
            "ticks_of_quarantine must be at least 1"
        );
        let to_quarantine: Arc<PagesToQuarantine> = Default::default();
        Self {
            hand: AtomicU64::new(0),
            slots: (0..slots)
                .map(|_| Mutex::new(Page::new(&to_quarantine)))
                .collect(),
            to_quarantine: to_quarantine,
            tick_state: Mutex::new(TickState::new(ticks_of_quarantine)),
        }
    }

    /// Advances the hand to the next slot. Interrupts stores watching the page
    /// there.
    ///
    /// Until this is first called, nothing is interrupted.
    pub fn tick(&self) {
        let TickState { free, quarantines } = &mut *self.tick_state.lock().unwrap();

        // tick() is the only writer of hand, so we can grab its value, add 1
        // now, and store it later.
        let next_hand = self.hand.load(Ordering::Relaxed).wrapping_add(1);

        let (reached_page, was_unwatched) = {
            // Ready a replacement page before taking slot lock, since unprotecting
            // takes awhile (taking a per-process lock).
            let replacement_page = match free.pop_front() {
                Some(page) => {
                    page.unprotect();
                    page.reached_and_refcount.store(0, Ordering::Release);
                    page
                }
                None => Page::new(&self.to_quarantine),
            };

            let mut slot = self.slot(next_hand).lock().unwrap();
            // Set the REACHED bit, and get ready to protect the page. It may
            // have stores watching it. It may have stale in-register caches
            // pointing to it, even if the refcount is 0.
            let old_reached_and_refcount = slot
                .reached_and_refcount
                .fetch_or(REACHED, Ordering::AcqRel);
            // We drop our reference, but stores may still be hanging on.
            (
                core::mem::replace(&mut *slot, replacement_page),
                old_reached_and_refcount & REFCOUNT_MASK == 0,
            )
        };

        // Empty quarantine bucket whose time is up.
        let bucket = wraparound(next_hand, quarantines.len());
        free.extend(quarantines[bucket].drain(..));

        // Put to_quarantine pages into the proper quarantine bucket.
        quarantines[bucket].extend(self.to_quarantine.lock().unwrap().drain(..));

        // This must come after the replace(). It's the replace() that keeps
        // future acquire_page() callers from getting ahold of the old page.
        self.hand.store(next_hand, Ordering::Release);

        // Keep this expensive part outside the slot lock.
        reached_page.protect();

        // No store ever watched this page, or they all released it before it
        // was reached. Either way, PageHandle::drop() will not quarantine it.
        // For efficiency (page reuse) and correctness (not crashing if a stale
        // interrupt page ptr references it), recycle such pages here.
        if was_unwatched {
            quarantines[bucket].push(reached_page);
        }
    }

    /// Returns the slot at the given hand position.
    fn slot(&self, hand: u64) -> &Mutex<Arc<Page>> {
        &self.slots[wraparound(hand, self.slots.len())]
    }

    /// Returns the furthest-future slot: the one that will be reached after
    /// every other one.
    fn furthest_slot(&self) -> &Mutex<Arc<Page>> {
        self.slot(self.hand.load(Ordering::Acquire))
    }
}

impl MmuInterrupter for TimingWheelInterrupter {
    /// Hands out a page from the furthest-in-the-future slot.
    fn acquire_page(&self) -> Box<dyn super::PageHandle> {
        let page = {
            let page = self.furthest_slot().lock().unwrap();
            let prev = page.reached_and_refcount.fetch_add(1, Ordering::AcqRel);
            assert_ne!(
                prev & REFCOUNT_MASK,
                REFCOUNT_MASK,
                // It overflowed.
                "too many stores share an interrupt page"
            );
            page.clone()
        };
        // Keep heap allocation outside the slot lock.
        Box::new(PageHandle(page))
    }
}

struct PageHandle(Arc<Page>);

impl super::PageHandle for PageHandle {
    fn page_ptr(&self) -> NonNull<c_void> {
        self.0.mmap.as_non_null().cast()
    }
}

impl Drop for PageHandle {
    /// If this page was reached by the timing wheel and we're the last holder,
    /// retire it into the quarantine.
    fn drop(&mut self) {
        let page = &self.0;
        // A failed upgrade() is okay: if the interrupter is gone, so is every
        // Engine that could load from the page, and nothing is watching it.
        if page.reached_and_refcount.fetch_sub(1, Ordering::AcqRel) == REACHED + 1
            && let Some(to_quarantine) = page.to_quarantine.upgrade()
        {
            // We were the last holder.
            to_quarantine.lock().unwrap().push(page.clone());
        }
    }
}

type PagesToQuarantine = Mutex<Vec<Arc<Page>>>;

/// A bit of `Page::reached_and_refcount` which marks a `Page` as having been
/// reached by the hand of the timing wheel and severed, reference-wise, from
/// the wheel. Its purpose is to disclaim the wheel's hold on it to
/// `PageHandle`'s drop impl so the last dropper of it can queue it for
/// recycling. Otherwise, we'd have to do something less efficient there like
/// identifying and locking the applicable slot and making sure the page isn't
/// there.
const REACHED: u32 = 1 << 31;
/// Masks the number of stores holding a page out of `Page::reached_and_refcount`
const REFCOUNT_MASK: u32 = REACHED - 1;

struct Page {
    /// Unmapped on drop, which happens only once neither the interrupter nor
    /// any `PageHandle` holds the page, so no Engine can load from it.
    mmap: Mmap<AlignedLength>,
    /// `REACHED` plus the number of stores holding this page. They share one
    /// atomic so `tick()` can mark the page reached and read its refcount in a
    /// single step; otherwise, a concurrent last release could miss the REACHED
    /// bit and never queue the page for unprotecting.
    reached_and_refcount: AtomicU32,
    to_quarantine: Weak<PagesToQuarantine>,
}

impl Page {
    fn new(to_quarantine: &Arc<PagesToQuarantine>) -> Arc<Page> {
        let mmap =
            Mmap::with_at_least(host_page_size()).expect("an interrupt page should be allocable");
        let page = Page {
            mmap,
            reached_and_refcount: AtomicU32::new(0),
            to_quarantine: Arc::downgrade(to_quarantine),
        };
        Arc::new(page)
    }

    fn protect(&self) {
        // SAFETY: Only Wasm loads from the page; Rust holds no references into it.
        unsafe { self.mmap.make_inaccessible(0..self.mmap.len()) }
            .expect("an interrupt page should be protectable");
    }

    fn unprotect(&self) {
        // SAFETY: as in protect(), above
        unsafe { self.mmap.make_readonly(0..self.mmap.len()) }
            .expect("an interrupt page should be unprotectable");
    }
}
