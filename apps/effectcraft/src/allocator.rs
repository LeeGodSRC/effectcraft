//! Give freed frame buffers back to the system (Linux with glibc).
//!
//! glibc's allocator raises its own "large block" threshold up to 32 MiB as blocks are freed,
//! and keeps everything below it in per-thread arenas. Frame rendering allocates and frees
//! pixel buffers of a few megabytes on many threads, so after a burst of frames the process
//! kept many gigabytes it no longer used: playing a heavy composition at full resolution
//! settled at 14 GB resident where the frames in flight needed 8 GB and the caches 2 GB, and
//! the system killed the app when memory ran out. A fixed threshold makes every buffer of
//! 1 MiB or more its own mapping, returned the moment it is freed.
//!
//! The allocator reads its tunables from the environment at program start, so the app starts
//! itself again once with them set (same process id; nothing has been opened yet).

/// Set on the restarted process: it does not start itself again.
const DONE: &str = "EFFECTCRAFT_ALLOCATOR_TUNED";
const TUNABLES: &str = "GLIBC_TUNABLES";
const THRESHOLD: &str = "glibc.malloc.mmap_threshold";
const WANTED: &str = "glibc.malloc.mmap_threshold=1048576:glibc.malloc.trim_threshold=67108864";

/// The `GLIBC_TUNABLES` value to restart with, or `None` to carry on: already restarted, or
/// the user chose a threshold themselves.
fn tunables(done: bool, current: Option<&str>) -> Option<String> {
    if done || current.is_some_and(|c| c.contains(THRESHOLD)) {
        return None;
    }
    Some(match current.filter(|c| !c.is_empty()) {
        Some(c) => format!("{c}:{WANTED}"),
        None => WANTED.to_string(),
    })
}

/// Restart with the allocator tunables unless they are in effect. Returns when there is
/// nothing to do or the restart failed (the app then runs as it is).
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn tune() {
    use std::os::unix::process::CommandExt;
    let current = std::env::var(TUNABLES).ok();
    let Some(value) = tunables(std::env::var_os(DONE).is_some(), current.as_deref()) else { return };
    let Ok(exe) = std::env::current_exe() else { return };
    // `exec` only returns on failure.
    let _ = std::process::Command::new(exe).args(std::env::args_os().skip(1)).env(TUNABLES, value).env(DONE, "1").exec();
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn tune() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tunables_are_added_once_and_never_override_the_users() {
        assert_eq!(tunables(false, None).as_deref(), Some(WANTED));
        assert_eq!(tunables(false, Some("")).as_deref(), Some(WANTED));
        assert_eq!(tunables(false, Some("glibc.malloc.arena_max=4")), Some(format!("glibc.malloc.arena_max=4:{WANTED}")));
        // Restarted already, or a threshold of the user's own: run as started.
        assert_eq!(tunables(true, None), None);
        assert_eq!(tunables(false, Some("glibc.malloc.mmap_threshold=65536")), None);
    }
}
