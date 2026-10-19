//! The browser build's heap: `dlmalloc` (what `std` uses on wasm) over a
//! system layer that grows linear memory in large steps.
//!
//! Every `memory.grow` costs time proportional to the memory's current size in
//! the browser (measured in Chrome: ~0.3 ms per grow at 500 MiB, ~6 ms at
//! 3 GiB, against 25 ms for one grow straight to 3 GiB). `std`'s allocator
//! grows by what each request needs, a 64 KiB granule at a time, so a load that
//! reaches 3 GiB spent most of its CPU time growing memory. Growing at least
//! [`GROW_STEP`] at a time keeps the count to a few dozen.

use core::alloc::{GlobalAlloc, Layout};
use core::arch::wasm32;
use core::cell::UnsafeCell;
use core::ptr;

const PAGE: usize = 64 * 1024;
const GROW_STEP: usize = 64 * 1024 * 1024;

struct ChunkedGrow;

fn grow(bytes: usize) -> Option<(usize, usize)> {
    let pages = bytes.div_ceil(PAGE);
    let prev = wasm32::memory_grow(0, pages);
    (prev != usize::MAX).then_some((prev * PAGE, pages * PAGE))
}

unsafe impl dlmalloc::Allocator for ChunkedGrow {
    fn alloc(&self, size: usize) -> (*mut u8, usize, u32) {
        // A full step when it fits; near the 4 GiB ceiling, just the request.
        let Some((base, len)) = grow(size.max(GROW_STEP)).or_else(|| grow(size)) else {
            return (ptr::null_mut(), 0, 0);
        };
        // Rust pointers may not wrap to 0: hold back the last bytes of the
        // address space, as dlmalloc's own wasm layer does.
        let len = if base.wrapping_add(len) == 0 { len - 16 } else { len };
        (base as *mut u8, len, 0)
    }

    fn remap(&self, _ptr: *mut u8, _old: usize, _new: usize, _can_move: bool) -> *mut u8 {
        ptr::null_mut()
    }

    fn free_part(&self, _ptr: *mut u8, _old: usize, _new: usize) -> bool {
        false
    }

    fn free(&self, _ptr: *mut u8, _size: usize) -> bool {
        false
    }

    fn can_release_part(&self, _flags: u32) -> bool {
        false
    }

    fn allocates_zeros(&self) -> bool {
        true
    }

    fn page_size(&self) -> usize {
        PAGE
    }
}

/// One dlmalloc for the process. The single-threaded build runs every
/// allocation on one thread; the threaded build (shared memory, atomics)
/// takes [`lock`] around each call, as `std`'s wasm allocator does.
pub struct WasmHeap(UnsafeCell<dlmalloc::Dlmalloc<ChunkedGrow>>);

unsafe impl Sync for WasmHeap {}

/// A futex mutex over the heap: spin briefly, then sleep on the word. No
/// thread of the threaded build is the page's (the engine runs in a
/// worker), so sleeping is allowed everywhere; and without threads the lock
/// is never contended, so it never sleeps.
#[cfg(target_feature = "atomics")]
mod lock {
    use core::arch::wasm32;
    use core::sync::atomic::{AtomicI32, Ordering};

    const FREE: i32 = 0;
    const HELD: i32 = 1;
    const HELD_WAITED: i32 = 2;

    static STATE: AtomicI32 = AtomicI32::new(FREE);

    pub struct Guard;

    pub fn lock() -> Guard {
        if STATE
            .compare_exchange(FREE, HELD, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return Guard;
        }
        for _ in 0..64 {
            core::hint::spin_loop();
            if STATE
                .compare_exchange(FREE, HELD, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return Guard;
            }
        }
        loop {
            // Say a waiter exists, then sleep while the heap is held.
            let seen = STATE.swap(HELD_WAITED, Ordering::Acquire);
            if seen == FREE {
                return Guard;
            }
            unsafe {
                wasm32::memory_atomic_wait32(STATE.as_ptr(), HELD_WAITED, -1);
            }
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            if STATE.swap(FREE, Ordering::Release) == HELD_WAITED {
                unsafe {
                    wasm32::memory_atomic_notify(STATE.as_ptr(), 1);
                }
            }
        }
    }
}

#[cfg(not(target_feature = "atomics"))]
mod lock {
    pub struct Guard;

    #[inline(always)]
    pub fn lock() -> Guard {
        Guard
    }
}

impl WasmHeap {
    pub const fn new() -> Self {
        Self(UnsafeCell::new(dlmalloc::Dlmalloc::new_with_allocator(
            ChunkedGrow,
        )))
    }
}

unsafe impl GlobalAlloc for WasmHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _held = lock::lock();
        unsafe { (*self.0.get()).malloc(layout.size(), layout.align()) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let _held = lock::lock();
        unsafe { (*self.0.get()).calloc(layout.size(), layout.align()) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let _held = lock::lock();
        unsafe { (*self.0.get()).free(ptr, layout.size(), layout.align()) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _held = lock::lock();
        unsafe { (*self.0.get()).realloc(ptr, layout.size(), layout.align(), new_size) }
    }
}
