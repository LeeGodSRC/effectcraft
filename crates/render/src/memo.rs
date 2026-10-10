//! Precomp frames shared within one frame.
//!
//! A nested composition's frame depends only on the comp, its time, the render scale and the
//! switches it inherits — not on the precomp layer that shows it. A comp used by several layers
//! at the same time (a tile repeated along a track, one comp behind several 3D planes) is
//! therefore the same picture every time, and [`FrameMemo`] keeps it for the rest of the
//! top-level frame instead of rendering it once per layer.
//!
//! The memo lives for one top-level frame ([`crate::Renderer::comp_frame_cpu`]), so nothing in
//! it can outlive an edit to the project. It never blocks: two threads that miss the same key
//! at once both render it (to the same pixels). Layers drawn in parallel avoid that by rendering
//! their shared frames first ([`crate::Renderer::prewarm_precomps`]).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use effectcraft_project::{ItemId, Quality};
use effectcraft_raster::{Image, Px};
use effectcraft_time::Tick;

/// Pixels kept per top-level frame (bytes); frames past it are rendered again when needed.
const BUDGET: usize = 512 << 20;

/// Everything a nested comp's frame depends on within one top-level frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Key {
    comp: ItemId,
    time: Tick,
    /// Render scale (bits).
    scale: u64,
    /// Nesting depth (the recursion limits count it).
    depth: usize,
    /// Quality (rank) and motion blur inherited from the precomp layers above.
    inherited: Option<(u8, bool)>,
}

impl Key {
    pub(crate) fn new(comp: ItemId, time: Tick, scale: f64, depth: usize, inherited: Option<(Quality, bool)>) -> Key {
        let rank = |q: Quality| match q {
            Quality::Best => 0,
            Quality::Draft => 1,
            Quality::Wireframe => 2,
        };
        Key { comp, time, scale: scale.to_bits(), depth, inherited: inherited.map(|(q, mb)| (rank(q), mb)) }
    }
}

#[derive(Default)]
struct Inner {
    frames: HashMap<Key, Arc<Image>>,
    /// Keys rendered once without being kept.
    seen: HashSet<Key>,
    bytes: usize,
}

/// See the module docs.
#[derive(Default)]
pub(crate) struct FrameMemo {
    inner: Mutex<Inner>,
}

impl FrameMemo {
    /// The frame kept for `key`.
    pub(crate) fn get(&self, key: &Key) -> Option<Arc<Image>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner).frames.get(key).cloned()
    }

    /// Note that `key` is about to be rendered; `true` when it was rendered before, so this
    /// render is worth keeping (a frame used once is not held for the rest of the frame).
    pub(crate) fn repeated(&self, key: Key) -> bool {
        !self.inner.lock().unwrap_or_else(PoisonError::into_inner).seen.insert(key)
    }

    /// Keep `img` for `key` (unless the memo is full or another thread got there first).
    pub(crate) fn insert(&self, key: Key, img: Arc<Image>) {
        let bytes = img.data.len().saturating_mul(std::mem::size_of::<Px>());
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if g.bytes.saturating_add(bytes) > BUDGET || g.frames.contains_key(&key) {
            return;
        }
        g.bytes += bytes;
        g.frames.insert(key, img);
    }
}
