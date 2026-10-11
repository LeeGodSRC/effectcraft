//! Layer render cache.
//!
//! The processed pixels of a layer (source → masks → effects, in layer space) depend only on the
//! layer's own non-transform properties, its source item and the render options — not on its
//! transform, opacity, blend mode or anything below it. [`LayerCache`] keeps those buffers keyed
//! by a content hash of exactly those inputs, *evaluated at the frame time*, so:
//!
//! * a static layer (or one whose only animation is in Transform) renders once and is reused on
//!   every frame while scrubbing or playing;
//! * editing one layer re-renders only that layer — every other layer's cached pixels stay valid
//!   because their keys do not change;
//! * any change to a property value, keyframe, expression result, effect, mask, switch or source
//!   item changes the key, so stale pixels are never returned.
//!
//! Layers whose pixels depend on time in ways the property values do not capture (time-based
//! effects such as Noise or Wave Warp, Wiggle Paths, footage, precomps) either fold the layer
//! time into the key or are not cached.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

use effectcraft_effects::Buf;
use effectcraft_project::{ItemKind, Layer, LayerSource, Node, PropGroup};

use crate::eval::EvalCtx;

/// Thread-safe, memory-budgeted cache of processed layer buffers, optionally backed by the
/// persistent [`DiskCache`](crate::disk_cache::DiskCache): a memory miss reads the disk, and
/// buffers that were slow to render are written there (keys salted per project, see
/// [`LayerCache::set_disk`]).
pub struct LayerCache {
    inner: Mutex<Inner>,
    disk: Mutex<Option<(Arc<crate::disk_cache::DiskCache>, u64)>>,
    /// Another persistent store when there is no disk cache (the browser's, [`PrefetchStore`]).
    store: Mutex<Option<(Arc<dyn LayerStore>, u64)>>,
    /// While set, inserts are dropped (see [`LayerCache::set_gate`]).
    gate: Mutex<Option<Arc<std::sync::atomic::AtomicBool>>>,
}

struct Entry {
    buf: Arc<Buf>,
    bytes: usize,
    last_use: u64,
    /// The key does not change from frame to frame (see [`LayerCache::insert_stable`]).
    stable: bool,
}

struct Inner {
    map: HashMap<u64, Entry>,
    bytes: usize,
    budget: usize,
    clock: u64,
    hits: u64,
    misses: u64,
    /// When each recent miss happened (to time the render that follows it).
    missed_at: HashMap<u64, web_time::Instant>,
}

/// Hit/miss counters and memory use (for perf readouts and tests).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub entries: usize,
    pub bytes: usize,
    pub hits: u64,
    pub misses: u64,
}

impl Default for LayerCache {
    fn default() -> Self {
        LayerCache::new(1 << 30)
    }
}

impl LayerCache {
    /// A cache holding at most `budget` bytes of pixels.
    pub fn new(budget: usize) -> LayerCache {
        LayerCache {
            inner: Mutex::new(Inner { map: HashMap::new(), bytes: 0, budget, clock: 0, hits: 0, misses: 0, missed_at: HashMap::new() }),
            disk: Mutex::new(None),
            store: Mutex::new(None),
            gate: Mutex::new(None),
        }
    }

    /// Drop inserts while `gate` is set. A render whose GPU readbacks are still in flight (the
    /// browser's deferred readbacks, see [`crate::Accelerator::pass_missed`]) computes some
    /// buffers from placeholders; the accelerator raises the gate from its first missing
    /// readback to the end of the pass, so none of them is kept (in memory or on disk).
    pub fn set_gate(&self, gate: Option<Arc<std::sync::atomic::AtomicBool>>) {
        if let Ok(mut g) = self.gate.lock() {
            *g = gate;
        }
    }

    fn gated(&self) -> bool {
        self.gate.lock().ok().and_then(|g| g.as_ref().map(|g| g.load(std::sync::atomic::Ordering::Relaxed))).unwrap_or(false)
    }

    /// Attach (or detach) the disk cache. `salt` separates projects and footage versions (see
    /// `footage_salt`); keys on disk are `(salt, key)`.
    pub fn set_disk(&self, disk: Option<(Arc<crate::disk_cache::DiskCache>, u64)>) {
        if let Ok(mut d) = self.disk.lock() {
            *d = disk;
        }
    }

    /// Change the disk salt (after footage changed) keeping the attached disk cache.
    pub fn set_disk_salt(&self, salt: u64) {
        if let Ok(mut d) = self.disk.lock()
            && let Some((_, s)) = d.as_mut()
        {
            *s = salt;
        }
    }

    pub fn disk(&self) -> Option<Arc<crate::disk_cache::DiskCache>> {
        self.disk.lock().ok()?.as_ref().map(|d| d.0.clone())
    }

    /// Attach (or detach) a [`LayerStore`] used when no disk cache is attached (the browser's
    /// Origin Private File System through a [`PrefetchStore`]); `salt` as in
    /// [`LayerCache::set_disk`].
    pub fn set_store(&self, store: Option<(Arc<dyn LayerStore>, u64)>) {
        if let Ok(mut s) = self.store.lock() {
            *s = store;
        }
    }

    /// Change the store's salt (a new project or footage), keeping the store.
    pub fn set_store_salt(&self, salt: u64) {
        if let Ok(mut d) = self.store.lock()
            && let Some((_, s)) = d.as_mut()
        {
            *s = salt;
        }
    }

    fn disk_key(&self, key: u64) -> Option<(Arc<dyn LayerStore>, u128)> {
        let salted = |salt: u64| ((salt as u128) << 64) | key as u128;
        if let Some((dc, salt)) = self.disk.lock().ok()?.as_ref() {
            return Some((dc.clone() as Arc<dyn LayerStore>, salted(*salt)));
        }
        let s = self.store.lock().ok()?;
        let (st, salt) = s.as_ref()?;
        Some((st.clone(), salted(*salt)))
    }

    pub fn get(&self, key: u64) -> Option<Arc<Buf>> {
        let mut g = self.inner.lock().ok()?;
        g.clock += 1;
        let now = g.clock;
        match g.map.get_mut(&key) {
            Some(e) => {
                e.last_use = now;
                let b = e.buf.clone();
                g.hits += 1;
                Some(b)
            }
            None => {
                g.misses += 1;
                if g.missed_at.len() > 4096 {
                    g.missed_at.clear();
                }
                g.missed_at.insert(key, web_time::Instant::now());
                drop(g);
                // Disk cache: a hit comes back into memory.
                let (dc, dk) = self.disk_key(key)?;
                let buf = dc.get_layer(dk)?;
                // A buffer too large for a one-frame entry is kept like one every frame uses,
                // whatever share of the budget it takes: dropped, each frame (and each layer
                // showing it) would read and unpack it from disk again, which is all a preview
                // of such a comp then did (minutes a frame, once the budget had been reduced).
                let bytes = buf.img.data.len() * std::mem::size_of::<effectcraft_raster::Px>();
                let large = self.inner.lock().is_ok_and(|g| bytes > g.budget / 4);
                self.insert_sized(key, buf.clone(), large, if large { 1 } else { 4 });
                Some(buf)
            }
        }
    }

    /// Whether the cache holds `key` in memory (not counted as a hit or a miss).
    pub fn holds(&self, key: u64) -> bool {
        self.inner.lock().is_ok_and(|g| g.map.contains_key(&key))
    }

    pub fn insert(&self, key: u64, buf: Arc<Buf>) {
        self.insert_as(key, buf, false);
    }

    /// [`Self::insert`] for a buffer whose key stays the same from frame to frame (a still
    /// layer, the frame of a comp nothing moves in): kept ahead of the buffers a frame uses once.
    pub fn insert_stable(&self, key: u64, buf: Arc<Buf>) {
        self.insert_as(key, buf, true);
    }

    fn insert_as(&self, key: u64, buf: Arc<Buf>, stable: bool) {
        if self.gated() {
            return;
        }
        let started = self.inner.lock().ok().and_then(|mut g| g.missed_at.remove(&key));
        if let Some((dc, dk)) = self.disk_key(key) {
            let slow = started.is_some_and(|t| t.elapsed().as_secs_f64() * 1e3 >= dc.min_layer_ms() as f64);
            if slow {
                dc.put_layer(dk, &buf);
            }
        }
        self.insert_mem(key, buf, stable);
    }

    fn insert_mem(&self, key: u64, buf: Arc<Buf>, stable: bool) {
        // One buffer may not crowd out the rest. A buffer every frame uses (an oversized precomp
        // nothing moves in) is worth more of the budget than one a single frame does: dropped,
        // it would be rendered again for every frame.
        self.insert_sized(key, buf, stable, if stable { 2 } else { 4 });
    }

    /// [`Self::insert_mem`] of a buffer that may take up to the budget over `share`.
    fn insert_sized(&self, key: u64, buf: Arc<Buf>, stable: bool, share: usize) {
        let bytes = buf.img.data.len() * std::mem::size_of::<effectcraft_raster::Px>();
        let Ok(mut g) = self.inner.lock() else { return };
        if bytes > g.budget / share.max(1) {
            return;
        }
        g.clock += 1;
        let now = g.clock;
        if let Some(old) = g.map.insert(key, Entry { buf, bytes, last_use: now, stable }) {
            g.bytes -= old.bytes;
        }
        g.bytes += bytes;
        if g.bytes > g.budget {
            // Evict down to 3/4 of the budget: the buffers one frame used first (least recently
            // used first), then the ones every frame can use. Playing a comp whose moving layers
            // fill the budget each frame would otherwise push out the still ones before the next
            // frame asks for them.
            // The buffers every frame can use only make room when they alone pass the budget.
            let target = g.budget / 4 * 3;
            let mut by_age: Vec<(bool, u64, u64)> = g.map.iter().map(|(k, e)| (e.stable, e.last_use, *k)).collect();
            by_age.sort_unstable();
            for (stable, _, k) in by_age {
                if g.bytes <= if stable { g.budget } else { target } {
                    break;
                }
                if let Some(e) = g.map.remove(&k) {
                    g.bytes -= e.bytes;
                }
            }
        }
    }

    /// The memory budget in bytes.
    pub fn budget(&self) -> usize {
        self.inner.lock().map(|g| g.budget).unwrap_or(0)
    }

    /// Change the memory budget (evicts least-recently used entries when shrinking).
    pub fn set_budget(&self, budget: usize) {
        let Ok(mut g) = self.inner.lock() else { return };
        g.budget = budget;
        if g.bytes > budget {
            let mut by_age: Vec<(u64, u64)> = g.map.iter().map(|(k, e)| (e.last_use, *k)).collect();
            by_age.sort_unstable();
            for (_, k) in by_age {
                if g.bytes <= budget {
                    break;
                }
                if let Some(e) = g.map.remove(&k) {
                    g.bytes -= e.bytes;
                }
            }
        }
    }

    pub fn clear(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.map.clear();
            g.bytes = 0;
        }
    }

    pub fn stats(&self) -> CacheStats {
        self.inner.lock().map(|g| CacheStats { entries: g.map.len(), bytes: g.bytes, hits: g.hits, misses: g.misses }).unwrap_or_default()
    }
}

/// Persistent storage of layer buffers behind a [`LayerCache`] (keys salted per project).
pub trait LayerStore: Send + Sync {
    /// A stored buffer (synchronous: mid-render).
    fn get_layer(&self, key: u128) -> Option<Arc<Buf>>;
    /// Keep a buffer that was slow to render.
    fn put_layer(&self, key: u128, buf: &Buf);
    /// Buffers faster to render than this (ms) are not stored.
    fn min_layer_ms(&self) -> u64 {
        0
    }
}

/// Layers buffers rendered slower than this go to the browser's disk cache (ms).
pub const PREFETCH_MIN_LAYER_MS: u64 = 20;

/// A [`LayerStore`] over storage that can't be read in the middle of a render (the browser's
/// Origin Private File System, read asynchronously). Lookups are served from buffers fetched
/// before the frame ([`PrefetchStore::provide`]); the keys a render looked up and didn't find
/// are recorded ([`PrefetchStore::take_misses`]) so the transport can fetch them before the next
/// frame; buffers to store queue up as sealed entries ([`PrefetchStore::take_writes`]) for the
/// transport to write after the frame. Fetched buffers are kept least recently used first, up
/// to a byte budget.
pub struct PrefetchStore {
    st: Mutex<Prefetched>,
    budget: usize,
    min_ms: u64,
}

#[derive(Default)]
struct Prefetched {
    ready: HashMap<u128, (Arc<Buf>, u64)>,
    bytes: usize,
    clock: u64,
    misses: Vec<u128>,
    hits: Vec<u128>,
    writes: Vec<(u128, Vec<u8>)>,
    /// Keys stored or queued (not written twice).
    known: std::collections::HashSet<u128>,
}

impl Default for PrefetchStore {
    fn default() -> Self {
        PrefetchStore::new(256 << 20)
    }
}

impl PrefetchStore {
    pub fn new(budget: usize) -> PrefetchStore {
        PrefetchStore { st: Mutex::new(Prefetched::default()), budget, min_ms: PREFETCH_MIN_LAYER_MS }
    }

    /// Store buffers that took at least `ms` to render (default [`PREFETCH_MIN_LAYER_MS`]).
    pub fn with_min_ms(mut self, ms: u64) -> PrefetchStore {
        self.min_ms = ms;
        self
    }

    /// Whether `key`'s buffer is here.
    pub fn has(&self, key: u128) -> bool {
        self.st.lock().is_ok_and(|s| s.ready.contains_key(&key))
    }

    /// A fetched entry (sealed bytes, [`crate::disk_cache::layer_entry`]) for `key`; `false`
    /// when damaged.
    pub fn provide(&self, key: u128, entry: &[u8]) -> bool {
        let Some(buf) = crate::disk_cache::read_layer_entry(entry) else { return false };
        let bytes = buf.img.data.len() * std::mem::size_of::<effectcraft_raster::Px>();
        let Ok(mut s) = self.st.lock() else { return false };
        s.clock += 1;
        let now = s.clock;
        if let Some((old, _)) = s.ready.insert(key, (Arc::new(buf), now)) {
            s.bytes -= old.img.data.len() * std::mem::size_of::<effectcraft_raster::Px>();
        }
        s.bytes += bytes;
        s.known.insert(key);
        while s.bytes > self.budget {
            let Some((&k, _)) = s.ready.iter().min_by_key(|(_, (_, t))| *t) else { break };
            if let Some((b, _)) = s.ready.remove(&k) {
                s.bytes -= b.img.data.len() * std::mem::size_of::<effectcraft_raster::Px>();
            }
        }
        true
    }

    /// Keys looked up and not found since the last call (each once).
    pub fn take_misses(&self) -> Vec<u128> {
        self.st.lock().map(|mut s| std::mem::take(&mut s.misses)).unwrap_or_default()
    }

    /// Keys served since the last call (each once).
    pub fn take_hits(&self) -> Vec<u128> {
        self.st.lock().map(|mut s| std::mem::take(&mut s.hits)).unwrap_or_default()
    }

    /// Entries to write: (key, sealed bytes).
    pub fn take_writes(&self) -> Vec<(u128, Vec<u8>)> {
        self.st.lock().map(|mut s| std::mem::take(&mut s.writes)).unwrap_or_default()
    }

    /// Forget everything (Empty Disk Cache, Purge).
    pub fn clear(&self) {
        if let Ok(mut s) = self.st.lock() {
            *s = Prefetched::default();
        }
    }
}

impl LayerStore for PrefetchStore {
    fn get_layer(&self, key: u128) -> Option<Arc<Buf>> {
        let mut s = self.st.lock().ok()?;
        s.clock += 1;
        let now = s.clock;
        match s.ready.get_mut(&key) {
            Some((b, t)) => {
                *t = now;
                let b = b.clone();
                if !s.hits.contains(&key) {
                    s.hits.push(key);
                }
                Some(b)
            }
            None => {
                if !s.misses.contains(&key) && s.misses.len() < 1024 {
                    s.misses.push(key);
                }
                None
            }
        }
    }

    fn put_layer(&self, key: u128, buf: &Buf) {
        let Ok(mut s) = self.st.lock() else { return };
        if s.known.insert(key) {
            s.writes.push((key, crate::disk_cache::layer_entry(buf)));
        }
    }

    fn min_layer_ms(&self) -> u64 {
        self.min_ms
    }
}

/// FNV-1a style 64-bit hasher fed by `Debug` output (exact float formatting) and raw values.
struct KeyHasher(u64);

impl Hasher for KeyHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
}

impl std::fmt::Write for KeyHasher {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        Hasher::write(self, s.as_bytes());
        Ok(())
    }
}

fn hash_debug(h: &mut KeyHasher, v: &impl std::fmt::Debug) {
    use std::fmt::Write;
    let _ = write!(h, "{v:?}");
    Hasher::write_u8(h, 0xff);
}

/// Fingerprint the full animation; expressions may depend on external mutable state.
pub(crate) fn effect_revision(group: &PropGroup) -> u64 {
    if has_expression(group) {
        return 0;
    }
    let mut h = KeyHasher(0xcbf2_9ce4_8422_2325);
    hash_debug(&mut h, group);
    h.finish().max(1)
}

/// Time-based layer content: does any part of the source/effects read the clock directly
/// (rather than through property values)?
pub(crate) fn time_dependent(layer: &Layer) -> bool {
    if layer.switches.effects
        && let Some(fx) = layer.effects()
    {
        for g in fx.groups() {
            if let effectcraft_project::GroupKind::Effect { effect } = &g.kind
                && g.enabled
                && effectcraft_effects::is_time_dependent(effect)
            {
                return true;
            }
            // Clone strokes reading another layer or another time.
            if g.enabled && effectcraft_effects::paint::is_paint(g) && effectcraft_effects::paint::cache_key(g, 0.0).1 {
                return true;
            }
        }
    }
    if matches!(layer.source, LayerSource::Shape)
        && let Some(c) = layer.props.sub("contents")
    {
        return group_has(c, "wiggle");
    }
    // Wiggly selectors move with time; Expression selectors evaluate per character (their
    // property value alone doesn't capture the result).
    if matches!(layer.source, LayerSource::Text)
        && let Some(t) = layer.props.sub("text")
    {
        return group_has(t, "wigglySelector") || group_has(t, "expressionSelector");
    }
    false
}

fn group_has(g: &PropGroup, match_id: &str) -> bool {
    g.children.iter().any(|c| match c {
        Node::Group(sub) => sub.match_id == match_id || group_has(sub, match_id),
        Node::Prop(_) => false,
    })
}

/// Feed every property of `g` that can change over time (keyframes or expression) as its value
/// at the context time. The static structure is hashed separately.
fn hash_values(h: &mut KeyHasher, ctx: &EvalCtx, layer: &Layer, g: &PropGroup) {
    for c in &g.children {
        match c {
            Node::Prop(p) => {
                if !p.keys.is_empty() || p.has_expression() {
                    p.uid.hash(h);
                    hash_debug(h, &ctx.value(layer, p));
                }
            }
            Node::Group(sub) => hash_values(h, ctx, layer, sub),
        }
    }
}

/// Cache key for the processed (source → masks → effects) buffer of `layer` at the context
/// time, or `None` when the layer must not be cached (footage, precomps, cameras, adjustment
/// layers…).
pub fn layer_key(ctx: &EvalCtx, layer: &Layer, scale: f64, draft: bool, blur: bool) -> Option<u64> {
    // Time effects see property values at other times. Keyframes are part of the hashed
    // structure (and the layer time is folded in), but expressions may read other layers at
    // those times, which the key cannot see: such layers are not cached.
    if reads_other_times(layer) && layer.props.children.iter().any(|c| matches!(c, Node::Group(g) if g.match_id != "transform" && has_expression(g))) {
        return None;
    }
    key_with(ctx, layer, scale, draft, blur, false)
}

/// Cache key for a layer's *input* at the context time: source → masks → its first `effects`
/// effects (see `Renderer::layer_input`). Unlike [`layer_key`], footage layers are cached here
/// (keyed by item and source time), since Time effects read many neighbouring frames.
pub fn input_key(ctx: &EvalCtx, layer: &Layer, scale: f64, draft: bool, blur: bool, effects: usize) -> Option<u64> {
    if layer.switches.adjustment {
        return None;
    }
    let base = key_any(ctx, layer, scale, draft, blur, true, Some(effects))?;
    let mut h = KeyHasher(base ^ 0x5bd1_e995_7a3c_11d3);
    effects.hash(&mut h);
    Some(h.finish())
}

/// Does the layer run an effect that reads the layer at other times (Echo, Timewarp…)?
fn reads_other_times(layer: &Layer) -> bool {
    layer.switches.effects
        && layer.effects().is_some_and(|fx| {
            fx.groups().any(|g| g.enabled && matches!(&g.kind, effectcraft_project::GroupKind::Effect { effect } if effect.starts_with("ec.time.")))
        })
}

fn has_expression(g: &PropGroup) -> bool {
    g.children.iter().any(|c| match c {
        Node::Prop(p) => p.has_expression(),
        Node::Group(sub) => has_expression(sub),
    })
}

/// Cache key for an adjustment layer's footprint (its source through its masks, see
/// `Renderer::adjustment_footprint`): the GPU compositor then uploads it once, not every frame.
/// The layer's effects are not part of it (they run on the comp below), so animating them
/// keeps the footprint.
pub fn footprint_key(ctx: &EvalCtx, layer: &Layer, scale: f64, draft: bool, blur: bool) -> Option<u64> {
    Some(derive(key_any(ctx, layer, scale, draft, blur, false, Some(0))?, 0xf007_9417))
}

fn key_with(ctx: &EvalCtx, layer: &Layer, scale: f64, draft: bool, blur: bool, footage: bool) -> Option<u64> {
    if layer.switches.adjustment {
        return None;
    }
    key_any(ctx, layer, scale, draft, blur, footage, None)
}

/// `blur`: the layer is motion blurred in this render (`Renderer::mb_on`). `effects`: key only
/// the first that many effects (a layer's input, see [`input_key`]).
#[allow(clippy::too_many_arguments)]
fn key_any(ctx: &EvalCtx, layer: &Layer, scale: f64, draft: bool, blur: bool, footage: bool, effects: Option<usize>) -> Option<u64> {
    let mut h = KeyHasher(0xcbf2_9ce4_8422_2325);
    match &layer.source {
        LayerSource::Solid { item } => {
            let it = ctx.project.item(*item)?;
            let ItemKind::Solid(s) = &it.kind else { return None };
            hash_debug(&mut h, s);
        }
        LayerSource::Text | LayerSource::Shape => {}
        // A still is the same picture at every time: its processed layer is cached like a
        // solid's (footage that plays is left to the footage cache).
        LayerSource::Footage { item } if !footage => {
            let it = ctx.project.item(*item)?;
            let ItemKind::Footage(f) = &it.kind else { return None };
            if it.proxy.is_some() || !still(f) || !f.has_video {
                return None;
            }
            hash_debug(&mut h, f);
            item.hash(&mut h);
            // Without masks or effects the layer's pixels are the still itself: every such layer
            // of the still shares one buffer.
            let bare = |g: Option<&PropGroup>| g.is_none_or(|g| g.children.is_empty());
            if bare(layer.masks()) && bare(layer.effects()) {
                crate::color::Pipe::of(&ctx.project.settings).key().hash(&mut h);
                scale.to_bits().hash(&mut h);
                return Some(h.finish());
            }
        }
        LayerSource::Footage { item } if footage => {
            let it = ctx.project.item(*item)?;
            let ItemKind::Footage(f) = &it.kind else { return None };
            hash_debug(&mut h, f);
            // A proxy (and its Use Proxy switch) changes the pixels.
            hash_debug(&mut h, &it.proxy);
            item.hash(&mut h);
            ctx.source_time(layer).0.hash(&mut h);
            // Frame blending mixes neighbouring source frames.
            ctx.comp.enable_frame_blending.hash(&mut h);
        }
        _ => return None,
    }
    // Bit depth and colour management change the pixels.
    crate::color::Pipe::of(&ctx.project.settings).key().hash(&mut h);
    ctx.comp_id.hash(&mut h);
    ctx.comp.width.hash(&mut h);
    ctx.comp.height.hash(&mut h);
    scale.to_bits().hash(&mut h);
    draft.hash(&mut h);
    layer.id.hash(&mut h);
    hash_debug(&mut h, &layer.source);
    // (Audio, Lock and Shy don't change pixels.)
    hash_debug(&mut h, &layer.switches.pixels());
    // Mask motion blur and effects that read the shutter depend on whether the layer is motion
    // blurred in this render and on the comp's shutter.
    if layer.masks().is_some_and(|m| !m.children.is_empty()) || layer.effects().is_some_and(|fx| !fx.children.is_empty()) {
        blur.hash(&mut h);
        ctx.comp.enable_motion_blur.hash(&mut h);
        ctx.comp.shutter_angle.to_bits().hash(&mut h);
        ctx.comp.shutter_phase.to_bits().hash(&mut h);
        ctx.comp.motion_blur_samples.hash(&mut h);
    }
    for c in &layer.props.children {
        // Transform is applied later; Layer Styles are keyed separately (see [`styles_key`]).
        if let Node::Group(g) = c
            && (g.match_id == "transform" || g.match_id == effectcraft_project::styles::GROUP)
        {
            continue;
        }
        // A layer's input runs only its first effects: the later ones (a Puppet whose pins are
        // dragged, #212) don't change it, so it stays cached.
        if let (Some(n), Node::Group(g)) = (effects, c)
            && g.match_id == "effects"
        {
            let mut first = g.clone();
            let mut seen = 0;
            first.children.retain(|e| match e {
                Node::Prop(_) => true,
                Node::Group(_) => {
                    seen += 1;
                    seen <= n
                }
            });
            hash_debug(&mut h, &first);
            hash_values(&mut h, ctx, layer, &first);
            continue;
        }
        // Static structure: values, keyframes, expressions, enabled flags, effect ids, modes.
        hash_debug(&mut h, c);
        if let Node::Group(g) = c {
            hash_values(&mut h, ctx, layer, g);
        }
    }
    // Paint strokes appear and disappear with their Duration spans (not property values).
    if layer.switches.effects
        && let Some(fx) = layer.effects()
    {
        let lt = layer.layer_time(ctx.time).seconds();
        for g in fx.groups().filter(|g| g.enabled && effectcraft_effects::paint::is_paint(g)) {
            effectcraft_effects::paint::cache_key(g, lt).0.hash(&mut h);
        }
    }
    if time_dependent(layer) {
        layer.layer_time(ctx.time).0.hash(&mut h);
        // Effects also see the layer's time mapping.
        layer.start_time.0.hash(&mut h);
        layer.stretch.to_bits().hash(&mut h);
    }
    // Effects that see the comp camera or lights (#397) change when they or the layer move.
    if reads_comp_scene(layer) {
        hash_debug(&mut h, &crate::comp_scene(ctx, layer));
    }
    Some(h.finish())
}

/// Does the layer run an effect that reads the comp camera or lights (Card Dance, CC Particle
/// World…)?
fn reads_comp_scene(layer: &Layer) -> bool {
    layer.switches.effects
        && layer.effects().is_some_and(|fx| {
            fx.groups().any(|g| {
                g.enabled
                    && matches!(&g.kind, effectcraft_project::GroupKind::Effect { effect } if effectcraft_effects::READS_COMP_SCENE.contains(&effect.as_str()))
            })
        })
}

/// Nesting limit of [`comp_key`] (deeper comps are not cached).
const MAX_COMP_KEY_DEPTH: usize = 32;

/// Content key of a whole composition frame — the comp and time of `ctx` — when the frame does
/// not depend on the time itself: nothing that takes part in it is animated, so the frame is
/// the same picture for as long as the same layers are shown. Two times with the same key show
/// the same pixels (at the same render options, which the caller adds), and any edit to the comp,
/// to a comp nested in it or to a source it uses changes the key.
///
/// Conservative: `None` (render it) when any layer that takes part is not still (see
/// [`still_layer`]). A layer that is not drawn (outside its in/out span, or switched off) and
/// that no drawn layer refers to takes no part.
pub fn comp_key(ctx: &EvalCtx, depth: usize) -> Option<u64> {
    if depth > MAX_COMP_KEY_DEPTH {
        return None;
    }
    let mut h = comp_hasher(ctx);
    for layer in &ctx.comp.layers {
        // Not drawn: outside its span, or switched off (cameras and lights have no switch).
        let hidden = !layer.is_active_at(ctx.time) || (layer.source.is_av() && !layer.switches.video && !layer.environment);
        if !hidden {
            still_layer(ctx, layer, depth)?.hash(&mut h);
        }
    }
    Some(h.finish())
}

/// Content key of the picture the first layers of `drawn` (a comp's layers in drawing order,
/// bottom first) make on an empty canvas, and how many of them: as many as are still (see
/// [`still_layer`]) and `eligible`. `None` when the bottom layer is not. As [`comp_key`], for the
/// part of a comp below its first moving layer.
pub fn prefix_key(ctx: &EvalCtx, drawn: &[&Layer], eligible: impl Fn(&Layer) -> bool) -> Option<(usize, u64)> {
    let mut parts = Vec::new();
    for layer in drawn {
        if !eligible(layer) {
            break;
        }
        let Some(k) = still_layer(ctx, layer, 0) else { break };
        parts.push(k);
    }
    if parts.is_empty() {
        return None;
    }
    let mut h = comp_hasher(ctx);
    0x9e37_79b9_u32.hash(&mut h);
    parts.hash(&mut h);
    Some((parts.len(), h.finish()))
}

/// A hasher holding the comp's whole static structure (settings, layers, properties, switches,
/// spans) and the colour pipeline.
fn comp_hasher(ctx: &EvalCtx) -> KeyHasher {
    let mut h = KeyHasher(0xcbf2_9ce4_8422_2325);
    hash_debug(&mut h, ctx.comp);
    crate::color::Pipe::of(&ctx.project.settings).key().hash(&mut h);
    ctx.comp_id.hash(&mut h);
    h
}

/// Content key of what `layer` draws at the time of `ctx` — with the layers it refers to
/// (parents, track matte, layer parameters of its effects), its source and the comp scene its
/// effects read — when that does not depend on the time itself. `None` when the layer or one it
/// refers to has keyframes, an expression, a time-dependent effect, a source that plays (video,
/// sequence, a proxy), Essential Properties overrides, or shows a nested comp whose own frame
/// has no such key.
fn still_layer(ctx: &EvalCtx, layer: &Layer, depth: usize) -> Option<u64> {
    let mut h = KeyHasher(0xcbf2_9ce4_8422_2325);
    // The layer and, in turn, every layer one of them refers to.
    let mut ids = vec![layer.id];
    let mut i = 0;
    while let Some(&id) = ids.get(i) {
        i += 1;
        // (A reference to a layer that is gone draws nothing.)
        let Some(l) = ctx.comp.layer(id) else { continue };
        let mut refs: Vec<u64> = Vec::new();
        refs.extend(l.parent.map(|p| p.0));
        refs.extend(l.track_matte.map(|m| m.layer.0));
        layer_refs(&l.props, &mut refs);
        for r in refs {
            let r = effectcraft_project::LayerId(r);
            if !ids.contains(&r) {
                ids.push(r);
            }
        }
        l.id.hash(&mut h);
        l.is_active_at(ctx.time).hash(&mut h);
        if animated(&l.props) || time_dependent(l) || reads_other_times(l) || !effectcraft_project::essential::overridden(l).is_empty() {
            return None;
        }
        if reads_comp_scene(l) {
            hash_debug(&mut h, &crate::comp_scene(ctx, l));
        }
        match &l.source {
            LayerSource::Solid { item } => {
                let ItemKind::Solid(sol) = &ctx.project.item(*item)?.kind else { return None };
                hash_debug(&mut h, sol);
            }
            LayerSource::Footage { item } => {
                let it = ctx.project.item(*item)?;
                let ItemKind::Footage(f) = &it.kind else { return None };
                // (Footage without pictures draws nothing.)
                if f.has_video && (it.proxy.is_some() || !still(f)) {
                    return None;
                }
                hash_debug(&mut h, f);
            }
            LayerSource::Comp { item } => {
                let it = ctx.project.item(*item)?;
                let nested = ctx.project.comp(*item)?;
                if it.proxy.is_some() || depth >= MAX_COMP_KEY_DEPTH || ctx.project.comp_contains(*item, ctx.comp_id) {
                    return None;
                }
                let at = EvalCtx { comp_id: *item, comp: nested, time: ctx.nested_time(l), ..*ctx };
                // Past the nested comp's span the layer shows nothing: the same at every such time.
                match nested.covers(at.time) {
                    true => comp_key(&at, depth + 1)?.hash(&mut h),
                    false => 0u8.hash(&mut h),
                }
            }
            // Text, shapes, cameras, lights and nulls are all in their properties.
            LayerSource::Text | LayerSource::Shape | LayerSource::Camera | LayerSource::Light { .. } | LayerSource::Null => {}
            // Other sources (models, primitives…) are left to render.
            _ => return None,
        }
    }
    Some(h.finish())
}

/// Whether a layer's processed pixels ([`layer_key`]) stay the same from frame to frame: nothing
/// but its transform is animated and no effect follows the time or the comp scene. The cache
/// keeps such buffers ahead of the ones a frame uses once ([`LayerCache::insert_stable`]).
pub fn stable_layer(layer: &Layer) -> bool {
    let moving = layer.props.children.iter().any(|c| match c {
        Node::Group(g) => g.match_id != "transform" && animated(g),
        Node::Prop(p) => !p.keys.is_empty() || p.has_expression(),
    });
    !moving && !time_dependent(layer) && !reads_other_times(layer) && !reads_comp_scene(layer)
}

/// A footage item whose picture is the same at every time.
fn still(f: &effectcraft_project::Footage) -> bool {
    f.kind == effectcraft_project::FootageKind::Still
}

/// Any keyframes or expression in the group.
pub(crate) fn animated(g: &PropGroup) -> bool {
    g.children.iter().any(|c| match c {
        Node::Prop(p) => !p.keys.is_empty() || p.has_expression(),
        Node::Group(sub) => animated(sub),
    })
}

/// The layers the group's layer parameters point at.
fn layer_refs(g: &PropGroup, out: &mut Vec<u64>) {
    for c in &g.children {
        match c {
            Node::Prop(p) => out.extend(p.value.as_layer()),
            Node::Group(sub) => layer_refs(sub, out),
        }
    }
}

/// Cache key for the styled layer (content key + the Layer Styles group: structure, eye
/// switches, values, keyframes, expressions and their values at the context time — Global Light
/// included, as every layer mirrors it in its Blending Options).
pub fn styles_key(ctx: &EvalCtx, layer: &Layer, content_key: u64) -> u64 {
    let mut h = KeyHasher(content_key ^ 0x9e37_79b9_7f4a_7c15);
    if let Some(g) = layer.layer_styles() {
        hash_debug(&mut h, g);
        hash_values(&mut h, ctx, layer, g);
    }
    h.finish()
}

/// A sub-key of `key` (styled layer passes, flattened buffer).
pub fn derive(key: u64, i: u64) -> u64 {
    let mut h = KeyHasher(key);
    i.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A store that has every key and counts the reads.
    struct Counting(AtomicUsize, u32);

    impl LayerStore for Counting {
        fn get_layer(&self, _key: u128) -> Option<Arc<Buf>> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Some(Arc::new(Buf { img: effectcraft_raster::Image::new(self.1, self.1), offset: [0.0; 2], scale: 1.0 }))
        }
        fn put_layer(&self, _key: u128, _buf: &Buf) {}
    }

    /// A buffer read from the disk store that is too large for a one-frame entry stays in
    /// memory (as the buffers every frame uses do): it was dropped, so every frame read and
    /// unpacked it again and a preview spent its time there.
    #[test]
    fn a_large_buffer_from_the_store_is_read_once() {
        // 40 × 40 × 16 bytes: over half of the budget.
        let cache = LayerCache::new(40_000);
        let store = Arc::new(Counting(AtomicUsize::new(0), 40));
        cache.set_store(Some((store.clone(), 7)));
        assert!(cache.get(1).is_some());
        assert!(cache.get(1).is_some());
        assert_eq!(store.0.load(Ordering::Relaxed), 1);
        // (One over the whole budget cannot be kept.)
        let big = Arc::new(Counting(AtomicUsize::new(0), 64));
        cache.set_store(Some((big.clone(), 7)));
        assert!(cache.get(2).is_some());
        assert!(cache.get(2).is_some());
        assert_eq!(big.0.load(Ordering::Relaxed), 2);
    }
}
