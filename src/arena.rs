//! Arena: shared-memory region header, allocator, and atomic primitives.
//!
//! All mutable shared state lives in the region and is accessed through
//! atomics so that multiple Node.js worker isolates can read/write the same
//! physical memory without data races.

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

pub const INIT_FRESH: u32 = 0;
pub const INIT_WRITING: u32 = 1;
pub const INIT_READY: u32 = 2;

#[cfg(test)]
pub const SIZE_HEADER_TEST: usize = SIZE_HEADER;

/// Byte offset where node records begin.
pub const SIZE_HEADER: usize = 48;

/// Sentinel for an empty free-list.
const FREE_EMPTY: usize = usize::MAX;

/// LOCK_FREE/lock values for the allocator spinlock stored in `_pad`.
const LOCK_FREE: u64 = 0;
const LOCK_HELD: u64 = 1;

/// Region metadata, laid out at the base of the shared memory region.
#[repr(C)]
pub struct RegionHeader {
    pub init_state: AtomicU32,
    pub backend: AtomicU32,
    pub capacity: AtomicUsize,
    pub bump: AtomicUsize,
    pub free_top: AtomicUsize,
    pub root: AtomicU64,
    /// Allocator spinlock (free under the `_pad` slot).
    pub _pad: AtomicU64,
}

impl RegionHeader {
    #[inline]
    fn from_base(base: *mut u8) -> &'static RegionHeader {
        unsafe { &*(base as *const RegionHeader) }
    }
}

/// Round a byte count up to an 8-byte boundary.
#[inline]
pub fn align_up(n: usize) -> usize {
    (n + 7) & !7
}

/// Initialize the region if it has not yet been initialized. Safe to call from
/// every isolate; only one thread runs the init while others spin.
///
/// # Safety
/// `base` + `capacity` must describe a valid, 8-aligned mapped/owned region.
pub unsafe fn ensure_init(base: *mut u8, capacity: usize, backend: u32) {
    let hdr = RegionHeader::from_base(base);
    loop {
        match hdr.init_state.load(Ordering::Acquire) {
            INIT_READY => return,
            INIT_WRITING => std::thread::yield_now(),
            _ => {
                if hdr
                    .init_state
                    .compare_exchange(INIT_FRESH, INIT_WRITING, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    hdr.backend.store(backend, Ordering::Release);
                    hdr.capacity.store(capacity, Ordering::Release);
                    hdr.bump.store(SIZE_HEADER, Ordering::Release);
                    hdr.free_top.store(FREE_EMPTY, Ordering::Release);
                    hdr.root.store(0, Ordering::Release);
                    hdr._pad.store(LOCK_FREE, Ordering::Release);
                    // Signal readiness (must be last).
                    hdr.init_state.store(INIT_READY, Ordering::Release);
                    return;
                }
            }
        }
    }
}

/// Acquire the allocation spinlock (blocks).
#[inline]
unsafe fn alloc_lock(base: *mut u8) {
    let hdr = RegionHeader::from_base(base);
    loop {
        if hdr
            ._pad
            .compare_exchange(LOCK_FREE, LOCK_HELD, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return;
        }
        std::thread::yield_now();
    }
}

/// Release the allocation spinlock.
#[inline]
unsafe fn alloc_unlock(base: *mut u8) {
    let hdr = RegionHeader::from_base(base);
    hdr._pad.store(LOCK_FREE, Ordering::Release);
}

/// Allocate `n` bytes (already 8-aligned) in the region: reuse a freed block
/// if available, otherwise bump allocate. Internally synchronized.
///
/// # Safety
/// `base` must point to an initialized region.
pub unsafe fn alloc_aligned(base: *mut u8, n: usize) -> Option<usize> {
    let hdr = RegionHeader::from_base(base);
    alloc_lock(base);
    let result = (|| {
        // Reuse the most recently freed block, if any.
        let top = hdr.free_top.load(Ordering::Relaxed);
        if top != FREE_EMPTY {
            let next = *(base.add(top) as *const usize);
            hdr.free_top.store(next, Ordering::Relaxed);
            return Some(top);
        }
        // Bump allocate.
        let cap = hdr.capacity.load(Ordering::Relaxed);
        loop {
            let cur = hdr.bump.load(Ordering::Relaxed);
            match cur.checked_add(n) {
                Some(end) if end <= cap => {
                    if hdr
                        .bump
                        .compare_exchange(cur, end, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                    {
                        return Some(cur);
                    }
                }
                _ => return None, // region full
            }
        }
    })();
    alloc_unlock(base);
    result
}

/// Free a block onto the free list. Internally synchronized.
///
/// # Safety
/// `off` must reference an allocated block that is no longer reachable.
#[allow(dead_code)]
pub unsafe fn dealloc(base: *mut u8, off: usize) {
    let hdr = RegionHeader::from_base(base);
    alloc_lock(base);
    let top = hdr.free_top.load(Ordering::Relaxed);
    *(base.add(off) as *mut usize) = top;
    hdr.free_top.store(off, Ordering::Relaxed);
    alloc_unlock(base);
}

/// Stable `&AtomicU64` view of a machine word in the region.
#[inline]
pub unsafe fn cell64<'a>(base: *mut u8, off: usize) -> &'a AtomicU64 {
    &*(base.add(off) as *const AtomicU64)
}

#[inline]
pub fn seq_is_odd(seq: u64) -> bool {
    (seq & 1) != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe fn test_region(size: usize) -> (*mut u8, usize) {
        // Allocate a private aligned buffer for single-process tests.
        use std::alloc::{alloc_zeroed, dealloc, Layout};
        let layout = Layout::from_size_align(size, 8).unwrap();
        let ptr = alloc_zeroed(layout) as *mut u8;
        ensure_init(ptr, size, 1);
        (ptr, layout.size())
    }

    #[test]
    fn alloc_and_free_reuse() {
        unsafe {
            let (base, size) = test_region(4096);
            let a = alloc_aligned(base, 120).unwrap();
            let b = alloc_aligned(base, align_up(64)).unwrap();
            assert_ne!(a, b);
            dealloc(base, a);
            let c = alloc_aligned(base, align_up(64)).unwrap();
            assert_eq!(c, a); // reuses the freed block
            let c = alloc_aligned(base, align_up(64)).unwrap();
            // free list now empty: bump picks up right after `b`.
            assert_eq!(c, a + 120 + align_up(64));
            let _ = size;
            drop((base, size));
        }
        let _ = SIZE_HEADER_TEST;
    }
}