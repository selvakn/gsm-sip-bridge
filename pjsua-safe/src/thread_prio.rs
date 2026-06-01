//! Real-time scheduling for PJMEDIA's audio threads.
//!
//! PJSUA/PJMEDIA run their sound-device threads (`alsasound_captu`, `alsasound_playb`, and
//! the `media`/`clock` timing threads) at `SCHED_OTHER`, so on a loaded or containerized
//! host they compete with everything else and the ALSA capture buffer under-/over-runs —
//! heard as choppy "noisy" GSM audio. This module promotes those threads of the *current
//! process* to `SCHED_FIFO` so the kernel services the audio path ahead of best-effort work.
//!
//! Promotion is best-effort: it requires `CAP_SYS_NICE` (granted by a privileged container)
//! and, on kernels built with `CONFIG_RT_GROUP_SCHED`, a non-zero cgroup RT budget. Failures
//! are logged, never fatal — the bridge keeps running at normal priority.
//!
//! Note: musl's `sched_setscheduler` libc wrapper is a stub that always returns `ENOSYS`, so
//! we invoke the `sched_setscheduler` syscall directly. This works on both glibc and musl.

/// Promote every thread of the current process whose `comm` name starts with one of
/// `name_prefixes` to `SCHED_FIFO` at priority `prio` (1–99; higher = more urgent). Prefix
/// matching tolerates the kernel's 15-char `comm` truncation (e.g. `alsasound_captu`).
///
/// Returns the number of threads successfully promoted. Logs each outcome, and — when no
/// thread matched — the available thread names so the caller can adjust the prefixes.
pub fn promote_threads_fifo(prio: i32, name_prefixes: &[&str]) -> usize {
    let task_dir = "/proc/self/task";
    let entries = match std::fs::read_dir(task_dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(target: "sip", error = %e, "could not enumerate {task_dir}; audio RT priority not applied");
            return 0;
        }
    };

    let mut matched = 0usize;
    let mut promoted = 0usize;
    let mut seen: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let tid: i32 = match entry.file_name().to_string_lossy().parse() {
            Ok(t) => t,
            Err(_) => continue,
        };
        let comm = std::fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
        let comm = comm.trim().to_string();
        if name_prefixes.iter().any(|n| comm.starts_with(n)) {
            matched += 1;
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
                        "failed to set SCHED_FIFO on audio thread (errno 1 = need CAP_SYS_NICE / RT cgroup budget)"
                    );
                }
            }
        }
        seen.push(comm);
    }

    if matched == 0 {
        tracing::warn!(
            target: "sip",
            wanted = ?name_prefixes,
            available = ?seen,
            "no audio thread matched; RT priority not applied"
        );
    } else if promoted < matched {
        tracing::warn!(
            target: "sip",
            matched, promoted,
            "some audio threads could not be promoted to real-time"
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
    // Call the syscall directly: musl's sched_setscheduler() wrapper is a stub returning
    // ENOSYS, so the libc wrapper would never actually reschedule the thread.
    // SAFETY: `tid` is a kernel thread id from /proc/self/task (a thread of this process);
    // `param` is a valid, initialized sched_param that outlives the call.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_sched_setscheduler,
            tid,
            libc::SCHED_FIFO,
            &param as *const libc::sched_param,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(-1))
    }
}
