//! Real-time scheduling for PJMEDIA's audio threads.
//!
//! PJSUA/PJMEDIA run their sound-device thread (named `media`) at `SCHED_OTHER`, so on a
//! loaded or containerized host it competes with every other thread and the ALSA capture
//! buffer under-/over-runs — heard as choppy "noisy" GSM audio. This module promotes the
//! relevant thread(s) of the *current process* to `SCHED_FIFO` so the kernel services the
//! audio path ahead of best-effort work.
//!
//! Promotion is best-effort: it requires `CAP_SYS_NICE` (granted by a privileged container)
//! and, on kernels built with `CONFIG_RT_GROUP_SCHED`, a non-zero cgroup RT budget. Failures
//! are logged, never fatal — the bridge keeps running at normal priority.

/// Promote every thread of the current process whose `comm` name matches one of `names`
/// to `SCHED_FIFO` at priority `prio` (1–99; higher = more urgent).
///
/// Returns the number of threads successfully promoted. When nothing matches, the
/// available thread names are logged so the caller can adjust `names` for their PJSIP build.
pub fn promote_threads_fifo(prio: i32, names: &[&str]) -> usize {
    let task_dir = "/proc/self/task";
    let entries = match std::fs::read_dir(task_dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(target: "sip", error = %e, "could not enumerate {task_dir}; audio RT priority not applied");
            return 0;
        }
    };

    let mut promoted = 0usize;
    let mut seen: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let tid: i32 = match entry.file_name().to_string_lossy().parse() {
            Ok(t) => t,
            Err(_) => continue,
        };
        let comm = std::fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
        let comm = comm.trim().to_string();
        if names.iter().any(|n| comm == *n) {
            match set_thread_fifo(tid, prio) {
                Ok(()) => {
                    promoted += 1;
                    tracing::info!(
                        target: "sip",
                        tid, thread = %comm, prio,
                        "promoted audio thread to SCHED_FIFO"
                    );
                }
                Err(errno) => {
                    tracing::warn!(
                        target: "sip",
                        tid, thread = %comm, prio, errno,
                        "failed to set SCHED_FIFO on audio thread (need CAP_SYS_NICE / RT cgroup budget)"
                    );
                }
            }
        }
        seen.push(comm);
    }

    if promoted == 0 {
        tracing::warn!(
            target: "sip",
            wanted = ?names,
            available = ?seen,
            "no audio thread matched; RT priority not applied"
        );
    }
    promoted
}

/// Apply `SCHED_FIFO` at `prio` to a single kernel thread id. Returns the OS errno on failure.
fn set_thread_fifo(tid: i32, prio: i32) -> Result<(), i32> {
    // Zero-initialize then set only `sched_priority`: `sched_param` is a C POD whose layout
    // differs across libc implementations (musl adds `sched_ss_*` fields), so a struct
    // literal naming a single field fails to compile on musl. Zeroing is correct for the
    // unused fields under SCHED_FIFO.
    // SAFETY: `sched_param` is plain old data for which an all-zero bit pattern is valid.
    let mut param: libc::sched_param = unsafe { std::mem::zeroed() };
    param.sched_priority = prio;
    // SAFETY: `tid` is a kernel thread id read from /proc/self/task (a thread of this
    // process); `param` is a valid, initialized sched_param living for the call.
    let rc = unsafe { libc::sched_setscheduler(tid, libc::SCHED_FIFO, &param) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(-1))
    }
}
