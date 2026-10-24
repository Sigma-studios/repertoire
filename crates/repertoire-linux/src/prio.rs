//! Making background work polite: lowest CPU priority and idle I/O class,
//! so a rescan never makes a game stutter.

/// Lower the calling thread's CPU and I/O priority. On Linux both are
/// per-thread when the target is 0.
pub fn idle_current_thread() {
    // SAFETY: plain syscalls on the calling thread; failures are harmless.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 19);
        // ioprio_set(IOPRIO_WHO_PROCESS, 0, IOPRIO_CLASS_IDLE << IOPRIO_CLASS_SHIFT)
        libc::syscall(libc::SYS_ioprio_set, 1, 0, 3 << 13);
    }
}
