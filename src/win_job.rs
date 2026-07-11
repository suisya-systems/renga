//! Windows Job Object wrapper for pane process-tree lifetime management.
//!
//! `Pane::kill` historically relied on `taskkill /F /T`, which walks
//! live parent→child links only. Two real leak paths escape it:
//!
//! * an intermediate process (e.g. Claude's `node.exe`) exits first,
//!   so its children (dev servers, `run_in_background` jobs, …) are
//!   unreachable from the pane shell's tree walk and survive close;
//! * the shell itself has already exited, in which case the taskkill
//!   path was skipped entirely and orphaned grandchildren were never
//!   reaped.
//!
//! A Job Object closes both holes: every process spawned (transitively)
//! by the pane shell is placed in the job by the kernel at creation
//! time, independent of the parent/child link surviving, and
//! `TerminateJobObject` kills all current members in one call.
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` backstops the explicit
//! terminate: even if renga dies without running `Pane::kill`, the
//! last handle close reaps the job. Nested jobs are supported since
//! Windows 8, so a pane job coexists with any job the host terminal
//! or CI runner already put renga in.

use std::ffi::c_void;

type Handle = isize;

const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
/// `JOBOBJECTINFOCLASS::JobObjectExtendedLimitInformation`
const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS: u32 = 9;
const PROCESS_SET_QUOTA: u32 = 0x0100;
const PROCESS_TERMINATE: u32 = 0x0001;

#[repr(C)]
#[derive(Default)]
struct JobObjectBasicLimitInformation {
    per_process_user_time_limit: i64,
    per_job_user_time_limit: i64,
    limit_flags: u32,
    minimum_working_set_size: usize,
    maximum_working_set_size: usize,
    active_process_limit: u32,
    affinity: usize,
    priority_class: u32,
    scheduling_class: u32,
}

#[repr(C)]
#[derive(Default)]
struct IoCounters {
    read_operation_count: u64,
    write_operation_count: u64,
    other_operation_count: u64,
    read_transfer_count: u64,
    write_transfer_count: u64,
    other_transfer_count: u64,
}

#[repr(C)]
#[derive(Default)]
struct JobObjectExtendedLimitInformation {
    basic_limit_information: JobObjectBasicLimitInformation,
    io_info: IoCounters,
    process_memory_limit: usize,
    job_memory_limit: usize,
    peak_process_memory_used: usize,
    peak_job_memory_used: usize,
}

extern "system" {
    fn CreateJobObjectW(lp_job_attributes: *mut c_void, lp_name: *const u16) -> Handle;
    fn SetInformationJobObject(
        h_job: Handle,
        job_object_information_class: u32,
        lp_job_object_information: *const c_void,
        cb_job_object_information_length: u32,
    ) -> i32;
    fn AssignProcessToJobObject(h_job: Handle, h_process: Handle) -> i32;
    fn TerminateJobObject(h_job: Handle, u_exit_code: u32) -> i32;
    fn OpenProcess(dw_desired_access: u32, b_inherit_handle: i32, dw_process_id: u32) -> Handle;
    fn CloseHandle(h_object: Handle) -> i32;
}

/// Owns one Job Object handle whose membership is a pane's shell plus
/// every descendant the kernel added since assignment. Dropping the
/// handle kills the members via `KILL_ON_JOB_CLOSE`; call
/// [`PaneJob::terminate`] first for a deterministic, synchronous kill.
pub struct PaneJob(Handle);

// SAFETY: the wrapped value is a kernel HANDLE, which is process-global
// and safe to use from any thread; the OS serializes operations on it.
unsafe impl Send for PaneJob {}
unsafe impl Sync for PaneJob {}

impl PaneJob {
    /// Create a kill-on-close job and put `pid` (and, transitively, its
    /// future descendants) in it. Returns `None` when any step fails —
    /// e.g. the process died during the call — in which case the caller
    /// keeps the legacy `taskkill /F /T` fallback.
    ///
    /// Assignment happens right after spawn, not atomically with it
    /// (portable-pty doesn't expose `CREATE_SUSPENDED`), so a child the
    /// shell spawns inside that microsecond window would escape the
    /// job. Shell init takes orders of magnitude longer than the gap,
    /// and `pending_startup` commands flush only after a prompt is
    /// seen, so the race is theoretical.
    pub fn assign(pid: u32) -> Option<Self> {
        unsafe {
            let job = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
            if job == 0 {
                return None;
            }
            let job = PaneJob(job);

            let mut info = JobObjectExtendedLimitInformation::default();
            info.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                job.0,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS,
                (&info as *const JobObjectExtendedLimitInformation).cast(),
                std::mem::size_of::<JobObjectExtendedLimitInformation>() as u32,
            );
            if ok == 0 {
                return None; // job handle closed by Drop
            }

            let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
            if process == 0 {
                return None;
            }
            let assigned = AssignProcessToJobObject(job.0, process);
            CloseHandle(process);
            if assigned == 0 {
                return None;
            }
            Some(job)
        }
    }

    /// Synchronously kill every process currently in the job. Errors
    /// are ignored: the only recovery would be the `KILL_ON_JOB_CLOSE`
    /// backstop that runs on drop anyway.
    pub fn terminate(&self) {
        unsafe {
            let _ = TerminateJobObject(self.0, 1);
        }
    }
}

impl Drop for PaneJob {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    fn wait_exited(child: &mut std::process::Child, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// Liveness probe for a grandchild we can't identify by pid: the
    /// grandchild holds `path` open with sharing mode `None`, so as
    /// long as it lives, every other open attempt fails with a sharing
    /// violation. Kernel-enforced, so it works in sandboxed test
    /// environments where signal-based probes (`waitfor /si`) don't.
    fn lock_is_held(path: &std::path::Path) -> bool {
        std::fs::OpenOptions::new().write(true).open(path).is_err()
    }

    /// Spawn `cmd → (start /b) powershell` where powershell exclusively
    /// locks `lock_path` and sleeps. cmd exits immediately, orphaning
    /// the powershell grandchild — the exact tree shape `taskkill /T`
    /// cannot reach.
    fn spawn_locker_grandchild(lock_path: &std::path::Path) -> std::process::Child {
        use std::os::windows::process::CommandExt;
        std::fs::write(lock_path, b"x").expect("create lock file");
        // `raw_arg` bypasses Rust's argv quoting, which cmd.exe's `/c`
        // parser does not understand.
        let ps = format!(
            "$f=[IO.File]::Open('{}','Open','ReadWrite','None'); Start-Sleep 60",
            lock_path.display()
        );
        Command::new("cmd.exe")
            .arg("/c")
            .raw_arg(format!("start /b powershell -NoProfile -Command \"{ps}\""))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn cmd tree")
    }

    fn wait_for(mut cond: impl FnMut() -> bool, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    #[test]
    fn assign_rejects_dead_pid() {
        // PID 0 is the idle process; OpenProcess must fail.
        assert!(PaneJob::assign(0).is_none());
    }

    #[test]
    fn terminate_kills_direct_child() {
        let mut child = Command::new("waitfor.exe")
            .args(["rengatrxdirect", "/t", "60"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn waitfor");
        let job = PaneJob::assign(child.id()).expect("assign job");
        job.terminate();
        assert!(
            wait_exited(&mut child, Duration::from_secs(5)),
            "direct child should die on TerminateJobObject"
        );
    }

    #[test]
    fn terminate_kills_grandchild_whose_parent_exited() {
        let lock_path =
            std::env::temp_dir().join(format!("rengatrx-job-{}.lock", std::process::id()));
        let mut cmd = spawn_locker_grandchild(&lock_path);
        let job = PaneJob::assign(cmd.id()).expect("assign job");
        // Positive precondition: the grandchild is up and holding the
        // lock (also proves the probe works), and the intermediate cmd
        // has exited so only the orphaned grandchild remains — the
        // exact shape `taskkill /T` cannot reach.
        assert!(
            wait_for(|| lock_is_held(&lock_path), Duration::from_secs(10)),
            "grandchild should start and hold the lock"
        );
        assert!(
            wait_exited(&mut cmd, Duration::from_secs(10)),
            "cmd wrapper should exit quickly"
        );
        job.terminate();
        assert!(
            wait_for(|| !lock_is_held(&lock_path), Duration::from_secs(5)),
            "grandchild should be dead after job termination"
        );
        let _ = std::fs::remove_file(&lock_path);
    }
}
