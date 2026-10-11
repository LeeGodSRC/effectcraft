//! Admission of frame renders by free system memory.
//!
//! Frame threads start many frames at once, and one frame of a heavy composition holds
//! gigabytes of working buffers that no cache budget counts. Rendering as many frames as
//! there are threads then takes more memory than the machine has, and the system kills the
//! app. A frame therefore starts only while the system has room for one more: the first
//! always does, further ones wait for memory or for a running frame to finish.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use effectcraft_engine::sysinfo::SysMemory;
use web_time::Instant;

/// Working memory assumed for a frame before any was measured.
const FIRST_GUESS: u64 = 512 << 20;
/// A frame that started this recently has not reached its peak yet: its share is still owed.
const WARM_UP: Duration = Duration::from_secs(3);
/// How long a memory reading is trusted.
const READING_AGE: Duration = Duration::from_millis(300);

/// Memory left to the rest of the system: an eighth of it, at least 2 GiB.
fn reserve(total: u64) -> u64 {
    (total / 8).max(2 << 30)
}

/// Whether one more frame may start with `running` frames rendering, `warming` of them
/// started within [`WARM_UP`], each frame taking about `per_frame` bytes.
fn admits(running: usize, warming: usize, per_frame: u64, mem: Option<SysMemory>) -> bool {
    // The frame the viewer waits for always renders; unknown memory limits nothing.
    let Some(mem) = mem.filter(|m| m.total > 0 && running > 0) else { return true };
    let owed = per_frame.saturating_mul(warming as u64 + 1);
    mem.available >= reserve(mem.total).saturating_add(owed)
}

struct State {
    running: usize,
    /// The most memory a running frame was seen to take.
    per_frame: u64,
    /// Available memory when no frame was rendering.
    idle_available: u64,
    starts: VecDeque<Instant>,
    reading: Option<(Instant, Option<SysMemory>)>,
}

/// Shared by the frame threads; see the module documentation.
pub(crate) struct FrameMemory {
    state: Mutex<State>,
    wake: Condvar,
    read: fn() -> Option<SysMemory>,
}

/// One admitted frame; dropping it lets a waiting frame start.
pub(crate) struct FramePermit(Arc<FrameMemory>);

impl Drop for FramePermit {
    fn drop(&mut self) {
        let mut s = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        s.running = s.running.saturating_sub(1);
        drop(s);
        self.0.wake.notify_all();
    }
}

impl FrameMemory {
    pub(crate) fn new() -> Arc<Self> {
        Self::with_reader(effectcraft_engine::sysinfo::memory)
    }

    fn with_reader(read: fn() -> Option<SysMemory>) -> Arc<Self> {
        let state = State { running: 0, per_frame: FIRST_GUESS, idle_available: 0, starts: VecDeque::new(), reading: None };
        Arc::new(Self { state: Mutex::new(state), wake: Condvar::new(), read })
    }

    /// Frames rendering now.
    #[cfg(test)]
    fn running(&self) -> usize {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).running
    }

    /// Take in a memory reading: what the running frames use, or the idle level.
    fn learn(s: &mut State, mem: Option<SysMemory>) {
        let Some(mem) = mem else { return };
        if s.running == 0 {
            s.idle_available = mem.available;
        } else {
            let used = s.idle_available.saturating_sub(mem.available) / s.running as u64;
            s.per_frame = s.per_frame.max(used.min(mem.total / 2));
        }
    }

    /// Wait until one more frame may render.
    pub(crate) fn admit(self: &Arc<Self>) -> FramePermit {
        let mut s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            let now = Instant::now();
            let mem = match s.reading {
                Some((at, mem)) if now.duration_since(at) < READING_AGE => mem,
                _ => {
                    // Reading may start a system tool: not under the lock.
                    drop(s);
                    let mem = (self.read)();
                    s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
                    s.reading = Some((now, mem));
                    Self::learn(&mut s, mem);
                    mem
                }
            };
            while s.starts.front().is_some_and(|t| now.duration_since(*t) >= WARM_UP) {
                s.starts.pop_front();
            }
            if admits(s.running, s.starts.len(), s.per_frame, mem) {
                s.running += 1;
                s.starts.push_back(now);
                return FramePermit(self.clone());
            }
            s = self.wake.wait_timeout(s, Duration::from_millis(250)).map(|(s, _)| s).unwrap_or_else(|e| e.into_inner().0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn mem(available_gib: u64) -> Option<SysMemory> {
        Some(SysMemory { total: 64 * GIB, available: available_gib * GIB })
    }

    #[test]
    fn the_first_frame_always_renders_and_unknown_memory_limits_nothing() {
        assert!(admits(0, 0, 4 * GIB, mem(0)));
        assert!(admits(7, 7, 4 * GIB, None));
    }

    #[test]
    fn another_frame_needs_room_beyond_the_reserve_for_itself_and_the_warming_ones() {
        // 64 GiB machine: 8 GiB stay free. A 3 GiB frame fits in 11, not in 10.
        assert!(admits(1, 0, 3 * GIB, mem(11)));
        assert!(!admits(1, 0, 3 * GIB, mem(10)));
        // Two frames that only just started still owe their 3 GiB each.
        assert!(!admits(2, 2, 3 * GIB, mem(16)));
        assert!(admits(2, 2, 3 * GIB, mem(17)));
    }

    #[test]
    fn frames_that_take_gigabytes_stop_filling_memory() {
        // Sixteen threads ask at once on a machine with 19 GiB free where a frame takes 3 GiB:
        // the measured cost must hold the later ones back (this exhausted memory before).
        fn read() -> Option<SysMemory> {
            mem(19)
        }
        let gate = FrameMemory::with_reader(read);
        let first = gate.admit();
        {
            let mut s = gate.state.lock().unwrap();
            s.idle_available = 22 * GIB;
            FrameMemory::learn(&mut s, mem(19));
            assert_eq!(s.per_frame, 3 * GIB);
        }
        // 19 available - 8 reserved = 11: the first frame is still warming, so 6 are owed.
        let second = gate.admit();
        // Then 9 are owed for two warming frames and the next: it fits; the 12 of a fourth do not.
        let third = gate.admit();
        let waiting = gate.clone();
        let blocked = std::thread::spawn(move || drop(waiting.admit()));
        std::thread::sleep(Duration::from_millis(100));
        assert!(!blocked.is_finished(), "a fourth frame must wait");
        assert_eq!(gate.running(), 3);
        drop((first, second, third));
        blocked.join().unwrap();
        assert_eq!(gate.running(), 0);
    }
}
