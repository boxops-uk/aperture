//! Allocation-counting global allocator for the I9 guard
//! (`exec::scan_is_alloc_free_per_row`).
//!
//! Wraps the system allocator. When armed via [`count_allocs`], it tallies
//! allocations on the current thread so a test can assert the executor's per-row
//! hot path allocates nothing. Only `alloc`/`dealloc` are overridden; the
//! default `realloc`/`alloc_zeroed` route through `alloc`, so growth is counted
//! too.
//!
//! `#[cfg(test)]` only — a counting allocator must never ship.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
}

pub struct CountingAlloc;

// SAFETY: delegates every operation to the system allocator; the only added work
// is a thread-local counter bump, which allocates nothing.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ARMED.with(Cell::get) {
            ALLOCS.with(|c| c.set(c.get() + 1));
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

/// Run `f` with allocation counting armed on this thread, returning its result
/// and the number of allocations it made.
pub fn count_allocs<R>(f: impl FnOnce() -> R) -> (R, u64) {
    ARMED.with(|a| a.set(true));
    ALLOCS.with(|c| c.set(0));
    let result = f();
    ARMED.with(|a| a.set(false));
    let allocs = ALLOCS.with(Cell::get);
    (result, allocs)
}
