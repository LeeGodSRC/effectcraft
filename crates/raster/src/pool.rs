//! Recycled pixel buffers.
//!
//! A frame allocates and frees dozens of full-frame RGBA f32 images (a canvas per matte, a copy
//! per adjustment layer, scratch per effect). The system allocator maps and unmaps each one, so
//! every buffer is paged in again on first touch and unmapping it interrupts every other
//! rendering thread; with frames rendering in parallel that cost more than the pixels did. Large
//! buffers therefore come back here when an [`Image`](crate::Image) is dropped and are handed to
//! the next image of the same size.
//!
//! A recycled buffer keeps its old pixels: whoever takes one overwrites all of it.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use crate::Px;

/// Smaller buffers (under 1 MB) are the allocator's business.
const MIN_PIXELS: usize = 1 << 16;

/// Pixels kept waiting for reuse, in bytes.
const BUDGET: usize = 768 << 20;

struct Pool {
    /// Free buffers by length.
    free: BTreeMap<usize, Vec<Vec<Px>>>,
    bytes: usize,
}

static POOL: Mutex<Pool> = Mutex::new(Pool { free: BTreeMap::new(), bytes: 0 });

/// A buffer of exactly `n` pixels with unspecified contents, when one is waiting.
pub(crate) fn take(n: usize) -> Option<Vec<Px>> {
    if n < MIN_PIXELS {
        return None;
    }
    let mut g = POOL.lock().unwrap_or_else(PoisonError::into_inner);
    let list = g.free.get_mut(&n)?;
    let buf = list.pop()?;
    if list.is_empty() {
        g.free.remove(&n);
    }
    g.bytes = g.bytes.saturating_sub(n * std::mem::size_of::<Px>());
    Some(buf)
}

/// Keep `buf` for the next image of its size (dropped when the pool is full).
pub(crate) fn give(buf: Vec<Px>) {
    let n = buf.len();
    if n < MIN_PIXELS || buf.capacity() != n {
        return;
    }
    let bytes = n * std::mem::size_of::<Px>();
    let mut g = POOL.lock().unwrap_or_else(PoisonError::into_inner);
    if g.bytes + bytes > BUDGET {
        return;
    }
    g.bytes += bytes;
    g.free.entry(n).or_default().push(buf);
}

/// Free every waiting buffer (after a render job, or when memory is short).
pub fn trim() {
    let free = {
        let mut g = POOL.lock().unwrap_or_else(PoisonError::into_inner);
        g.bytes = 0;
        std::mem::take(&mut g.free)
    };
    drop(free);
}

/// Bytes waiting for reuse.
pub fn held() -> usize {
    POOL.lock().unwrap_or_else(PoisonError::into_inner).bytes
}
