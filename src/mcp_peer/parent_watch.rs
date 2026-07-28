//! Parent-process watchdog for `renga mcp-peer` (renga-9fs).
//!
//! The stdio loop's "exit on stdin EOF" contract is unreliable on
//! Windows: handle inheritance can leak the write end of our stdin
//! pipe into sibling children of the spawning client (Claude / Codex),
//! and any survivor holding that handle keeps EOF from ever arriving.
//! Observed in the wild as `renga mcp-peer` processes outliving their
//! parent by days once a `--bg-pty-host` daemon inherited the pipe.
//!
//! The watchdog makes parent death authoritative instead: when the
//! process that spawned us exits, we exit too, regardless of stdin.
//!
//! * **Windows**: open a `SYNCHRONIZE` handle to the parent once and
//!   block a thread on `WaitForSingleObject(…, INFINITE)`. The handle
//!   pins the process identity, so PID reuse after that point cannot
//!   confuse the wait. The only reuse window is between reading the
//!   parent PID and opening the handle; a creation-time comparison
//!   (parent must predate us) closes it.
//! * **Unix**: poll `getppid()`; when the original parent dies the
//!   kernel reparents us (to init/subreaper) and the value changes.
//!   stdin EOF already works there, so this is belt-and-braces.

use std::thread;

/// Spawn the watchdog thread. `on_parent_exit` runs at most once, on
/// a background thread, and only when a parent was positively
/// identified *and* subsequently observed to exit.
///
/// Arming is fallible — the process snapshot can fail transiently,
/// the parent handle can be unopenable, and the PID-reuse guard can
/// reject an impostor. Every such failure leaves the callback unfired
/// and the server running on the stdin-EOF path it used before this
/// module existed. Treating "could not observe the parent" as "the
/// parent died" would shut healthy servers down on a transient Win32
/// error, which is a worse failure than the leak this module fixes.
pub fn spawn(on_parent_exit: impl FnOnce() + Send + 'static) {
    let _ = thread::Builder::new()
        .name("parent-watchdog".into())
        .spawn(move || {
            if wait_parent_exit() {
                on_parent_exit();
            }
        });
}

/// Returns `true` only when a parent was watched and observed to
/// exit; `false` means the watchdog never armed.
#[cfg(windows)]
fn wait_parent_exit() -> bool {
    win::wait_parent_exit()
}

/// Returns `true` when reparenting is observed. `getppid` cannot
/// fail, so there is no arming-failure path to report here.
#[cfg(unix)]
fn wait_parent_exit() -> bool {
    // Known gap: if the parent died before this first read, `original`
    // is already the reaper's PID and the loop never fires. Unix has
    // working stdin EOF as the primary shutdown signal, so the
    // watchdog stays a best-effort backstop there — unlike Windows,
    // where it is authoritative.
    // SAFETY: getppid is async-signal-safe and has no failure mode.
    let original = unsafe { libc::getppid() };
    loop {
        std::thread::sleep(std::time::Duration::from_secs(2));
        let now = unsafe { libc::getppid() };
        if now != original {
            return true;
        }
    }
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;

    // Match `crate::conpty_colors::win`'s alias so the duplicated
    // extern declarations (WaitForSingleObject, CloseHandle, ...)
    // don't trip `clashing_extern_declarations`.
    type Handle = *mut c_void;

    const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const INFINITE: u32 = 0xFFFF_FFFF;
    const WAIT_OBJECT_0: u32 = 0;
    const MAX_PATH: usize = 260;

    #[repr(C)]
    struct ProcessEntry32W {
        dw_size: u32,
        cnt_usage: u32,
        th32_process_id: u32,
        th32_default_heap_id: usize,
        th32_module_id: u32,
        cnt_threads: u32,
        th32_parent_process_id: u32,
        pc_pri_class_base: i32,
        dw_flags: u32,
        sz_exe_file: [u16; MAX_PATH],
    }

    /// Win32 `FILETIME`. Field order is fixed by the ABI (low dword
    /// first), which is the REVERSE of numeric significance — never
    /// derive `Ord`/`PartialOrd` here; compare via [`FileTime::as_u64`].
    #[repr(C)]
    #[derive(Default, Clone, Copy, PartialEq, Eq)]
    pub(super) struct FileTime {
        dw_low_date_time: u32,
        dw_high_date_time: u32,
    }

    impl FileTime {
        pub(super) fn as_u64(self) -> u64 {
            (u64::from(self.dw_high_date_time) << 32) | u64::from(self.dw_low_date_time)
        }
    }

    extern "system" {
        fn CreateToolhelp32Snapshot(dw_flags: u32, th32_process_id: u32) -> Handle;
        fn Process32FirstW(h_snapshot: Handle, lppe: *mut ProcessEntry32W) -> i32;
        fn Process32NextW(h_snapshot: Handle, lppe: *mut ProcessEntry32W) -> i32;
        fn GetCurrentProcessId() -> u32;
        fn GetCurrentProcess() -> Handle;
        fn OpenProcess(dw_desired_access: u32, b_inherit_handle: i32, dw_process_id: u32)
            -> Handle;
        fn WaitForSingleObject(h_handle: Handle, dw_milliseconds: u32) -> u32;
        fn CloseHandle(h_object: Handle) -> i32;
        fn GetProcessTimes(
            h_process: Handle,
            lp_creation_time: *mut FileTime,
            lp_exit_time: *mut FileTime,
            lp_kernel_time: *mut FileTime,
            lp_user_time: *mut FileTime,
        ) -> i32;
    }

    /// Blocks until the parent process exits, then returns `true`.
    ///
    /// Returns `false` without blocking when the watchdog could not
    /// arm: no parent PID, no openable handle, or a PID-reuse
    /// impostor. All three mean "we cannot observe the parent", not
    /// "the parent died", so the caller must not treat them as a
    /// shutdown signal — `CreateToolhelp32Snapshot` in particular is
    /// documented to fail transiently (`ERROR_BAD_LENGTH`) when the
    /// process list churns mid-snapshot, and exiting on that would
    /// kill a healthy server.
    pub(super) fn wait_parent_exit() -> bool {
        // One retry after a short pause: `CreateToolhelp32Snapshot`
        // can fail transiently under resource pressure, and treating
        // that as "already orphaned" would exit a healthy server.
        let ppid = parent_pid().or_else(|| {
            std::thread::sleep(std::time::Duration::from_millis(500));
            parent_pid()
        });
        let Some(ppid) = ppid else {
            return false;
        };
        let Some(handle) = open_for_wait(ppid) else {
            // Could not open the parent. For a same-user parent that
            // usually means it is already gone, but it is not
            // distinguishable from a transient failure without
            // inspecting `GetLastError`, so disarm instead of
            // shutting down on a guess.
            return false;
        };
        // PID-reuse guard for the window between the snapshot and
        // OpenProcess: a genuine parent was created before us, so a
        // "parent" younger than us is a recycled PID — we are already
        // orphaned.
        if let (Some(parent_created), Some(self_created)) = (
            creation_time(handle),
            creation_time(unsafe { GetCurrentProcess() }),
        ) {
            if parent_created.as_u64() > self_created.as_u64() {
                unsafe { CloseHandle(handle) };
                return false;
            }
        }
        wait_and_close(handle);
        true
    }

    /// Construct a `FileTime` for tests without exposing the struct's
    /// fields outside this module.
    #[cfg(test)]
    pub(super) fn test_filetime(high: u32, low: u32) -> FileTime {
        FileTime {
            dw_low_date_time: low,
            dw_high_date_time: high,
        }
    }

    /// Block until the process identified by `pid` exits; returns
    /// immediately when it is already gone. Shared primitive for the
    /// parent wait above and the unit tests.
    #[cfg(test)]
    pub(super) fn wait_pid_exit(pid: u32) {
        if let Some(handle) = open_for_wait(pid) {
            wait_and_close(handle);
        }
    }

    fn open_for_wait(pid: u32) -> Option<Handle> {
        let handle =
            unsafe { OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        (!handle.is_null()).then_some(handle)
    }

    fn wait_and_close(handle: Handle) {
        let waited = unsafe { WaitForSingleObject(handle, INFINITE) };
        unsafe { CloseHandle(handle) };
        // WAIT_OBJECT_0 is the expected signal; treat WAIT_FAILED and
        // friends as "parent state unknowable" and return too —
        // staying alive un-watched is exactly the leak this module
        // prevents.
        let _ = waited == WAIT_OBJECT_0;
    }

    fn creation_time(handle: Handle) -> Option<FileTime> {
        let mut creation = FileTime::default();
        let mut exit = FileTime::default();
        let mut kernel = FileTime::default();
        let mut user = FileTime::default();
        let ok =
            unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
        (ok != 0).then_some(creation)
    }

    /// Find our parent PID via the documented Toolhelp snapshot API.
    pub(super) fn parent_pid() -> Option<u32> {
        let me = unsafe { GetCurrentProcessId() };
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot as isize == -1 || snapshot.is_null() {
            return None;
        }
        let mut entry = ProcessEntry32W {
            dw_size: std::mem::size_of::<ProcessEntry32W>() as u32,
            cnt_usage: 0,
            th32_process_id: 0,
            th32_default_heap_id: 0,
            th32_module_id: 0,
            cnt_threads: 0,
            th32_parent_process_id: 0,
            pc_pri_class_base: 0,
            dw_flags: 0,
            sz_exe_file: [0; MAX_PATH],
        };
        let mut found = None;
        unsafe {
            let entry_ptr: *mut ProcessEntry32W = &mut entry;
            if Process32FirstW(snapshot, entry_ptr) != 0 {
                loop {
                    if entry.th32_process_id == me {
                        found = Some(entry.th32_parent_process_id);
                        break;
                    }
                    if Process32NextW(snapshot, entry_ptr) == 0 {
                        break;
                    }
                }
            }
            CloseHandle(snapshot);
        }
        found
    }
}

#[cfg(test)]
#[cfg(windows)]
mod tests {
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::time::Duration;

    use super::win::wait_pid_exit;

    /// Regression for the PID-reuse guard: FILETIME's ABI field order
    /// (low dword first) is the reverse of numeric significance, so a
    /// derived lexicographic Ord would order these two values wrongly.
    /// The high dword rolls over every ~429.5 s, so "high differs,
    /// low compares the other way" is the common real-world shape —
    /// a parent 8+ minutes older than the child.
    #[test]
    fn filetime_comparison_uses_high_dword_first() {
        let older = super::win::test_filetime(5, 0xFFFF_FFFF); // high=5
        let newer = super::win::test_filetime(6, 0x0000_0001); // high=6
        assert!(older.as_u64() < newer.as_u64());
    }

    #[test]
    fn parent_pid_resolves() {
        // cargo's test runner is our parent; it must be discoverable.
        let ppid = super::win::parent_pid();
        assert!(ppid.is_some(), "parent pid should resolve via toolhelp");
        assert_ne!(ppid.unwrap(), std::process::id());
    }

    #[test]
    fn wait_returns_when_watched_process_dies() {
        let mut child = Command::new("waitfor.exe")
            .args(["renga9fswatch", "/t", "60"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn waitfor");
        let pid = child.id();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            wait_pid_exit(pid);
            let _ = tx.send(());
        });
        // Not signaled while the process lives.
        assert!(
            rx.recv_timeout(Duration::from_millis(800)).is_err(),
            "watchdog must not fire while the watched process is alive"
        );
        child.kill().expect("kill child");
        let _ = child.wait();
        assert!(
            rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "watchdog should fire promptly after the watched process dies"
        );
    }

    #[test]
    fn wait_returns_immediately_for_dead_process() {
        let mut child = Command::new("cmd.exe")
            .args(["/c", "exit"])
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn cmd");
        let pid = child.id();
        let _ = child.wait();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            wait_pid_exit(pid);
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "wait on an already-dead pid should return quickly"
        );
    }
}
