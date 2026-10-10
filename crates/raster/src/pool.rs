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
    /// Free buffers by capacity.
    free: BTreeMap<usize, Vec<Vec<Px>>>,
    bytes: usize,
}

static POOL: Mutex<Pool> = Mutex::new(Pool { free: BTreeMap::new(), bytes: 0 });

/// A buffer of exactly `n` pixels with unspecified contents, when one that size or a little
/// larger is waiting (a layer whose size animates asks for a different size every frame).
pub(crate) fn take(n: usize) -> Option<Vec<Px>> {
    if n < MIN_PIXELS {
        return None;
    }
    let mut buf = {
        let mut g = POOL.lock().unwrap_or_else(PoisonError::into_inner);
        let cap = *g.free.range(n..=n.saturating_add(n / 2)).next()?.0;
        let list = g.free.get_mut(&cap)?;
        let buf = list.pop()?;
        if list.is_empty() {
            g.free.remove(&cap);
        }
        g.bytes = g.bytes.saturating_sub(cap * std::mem::size_of::<Px>());
        buf
    };
    // (Within the capacity: no allocation. A buffer cut shorter before grows back with zeros.)
    buf.resize(n, [0.0; 4]);
    Some(buf)
}

/// Keep `buf` for the next image of about its size (dropped when the pool is full).
pub(crate) fn give(buf: Vec<Px>) {
    let cap = buf.capacity();
    if cap < MIN_PIXELS {
        return;
    }
    let bytes = cap * std::mem::size_of::<Px>();
    let mut g = POOL.lock().unwrap_or_else(PoisonError::into_inner);
    if g.bytes + bytes > BUDGET {
        return;
    }
    g.bytes += bytes;
    g.free.entry(cap).or_default().push(buf);
}

/// `n` pixels with unspecified contents (a recycled buffer's old pixels, or zeros), for a caller
/// that writes every one of them.
pub fn scratch(n: usize) -> Vec<Px> {
    take(n).unwrap_or_else(|| vec![[0.0; 4]; n])
}

/// Hand a pixel buffer that never became an [`Image`](crate::Image) back for reuse.
pub fn recycle(buf: Vec<Px>) {
    give(buf);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dropped_buffer_serves_the_next_image_of_about_its_size() {
        // (Sizes no other test uses: the pool is shared.)
        let n = MIN_PIXELS * 7 + 13;
        let buf = vec![[1.0f32; 4]; n];
        let at = buf.as_ptr();
        give(buf);
        // A little smaller: the same allocation, cut to size.
        let smaller = take(n - 100).expect("the buffer waits in the pool");
        assert_eq!((smaller.len(), smaller.as_ptr()), (n - 100, at));
        give(smaller);
        // Back at full size: the part cut off comes back as zeros.
        let again = take(n).expect("still waiting");
        assert_eq!((again.len(), again.as_ptr()), (n, at));
        assert!(again[n - 100..].iter().all(|p| *p == [0.0; 4]));
        // Far smaller requests do not hold on to a large buffer.
        give(again);
        assert!(take(n / 2).is_none());
        assert!(take(n).is_some());
    }

    #[test]
    fn a_recycled_image_starts_transparent() {
        let (w, h) = (640u32, 123u32 * 5 + 1);
        let mut img = crate::Image::new(w, h);
        img.data.iter_mut().for_each(|p| *p = [0.5; 4]);
        drop(img);
        assert!(crate::Image::new(w, h).data.iter().all(|p| *p == [0.0; 4]));
    }
}
