//! An owned process tree: a command and everything it starts, ended together.
//!
//! - **Windows:** the command starts suspended, is put into a Job Object with
//!   `KILL_ON_JOB_CLOSE`, then resumed, so not even its first instruction runs
//!   outside the job. Children and grandchildren join the job automatically
//!   and cannot break away. Closing the job, including when Lu itself exits
//!   or crashes, ends all of them.
//! - **Linux and other Unix:** the command leads a new process group, and the
//!   whole group is killed. A descendant that deliberately starts its own
//!   session (`setsid`) escapes the group; containing that needs cgroups,
//!   which is not built yet.
use std::process::{Child, Command};

#[cfg(windows)]
mod imp {
    use std::{io, mem, os::windows::io::AsRawHandle, os::windows::process::CommandExt, process::{Child, Command}, ptr};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE},
        System::{
            Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32},
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
                TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            },
            Threading::{OpenThread, ResumeThread, CREATE_NO_WINDOW, CREATE_SUSPENDED, THREAD_SUSPEND_RESUME},
        },
    };

    pub struct Tree {
        job: HANDLE,
    }
    // The job handle is only used through thread-safe kernel calls.
    unsafe impl Send for Tree {}
    unsafe impl Sync for Tree {}

    impl Tree {
        pub fn spawn(mut command: Command) -> io::Result<(Child, Tree)> {
            // SAFETY: plain Win32 calls with valid arguments; every handle is
            // checked and closed.
            unsafe {
                let job = CreateJobObjectW(ptr::null(), ptr::null());
                if job.is_null() {
                    return Err(io::Error::last_os_error());
                }
                let tree = Tree { job };
                let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = mem::zeroed();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &limits as *const _ as *const core::ffi::c_void,
                    mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ) == 0
                {
                    return Err(io::Error::last_os_error());
                }
                command.creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW);
                let mut child = command.spawn()?;
                if AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE) == 0 {
                    let error = io::Error::last_os_error();
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }
                if let Err(error) = resume_threads(child.id()) {
                    tree.kill();
                    let _ = child.wait();
                    return Err(error);
                }
                Ok((child, tree))
            }
        }

        /// End every process in the tree.
        pub fn kill(&self) {
            // SAFETY: the job handle is valid until drop.
            unsafe {
                TerminateJobObject(self.job, 1);
            }
        }
    }
    impl Drop for Tree {
        fn drop(&mut self) {
            // Closing the last handle ends the tree (KILL_ON_JOB_CLOSE).
            // SAFETY: closed exactly once.
            unsafe {
                CloseHandle(self.job);
            }
        }
    }

    /// Resume the suspended main thread of a just-created process.
    unsafe fn resume_threads(pid: u32) -> io::Result<()> {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let mut entry: THREADENTRY32 = mem::zeroed();
        entry.dwSize = mem::size_of::<THREADENTRY32>() as u32;
        let mut resumed = 0;
        let mut more = Thread32First(snapshot, &mut entry) != 0;
        while more {
            if entry.th32OwnerProcessID == pid {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if !thread.is_null() {
                    if ResumeThread(thread) != u32::MAX {
                        resumed += 1;
                    }
                    CloseHandle(thread);
                }
            }
            more = Thread32Next(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
        if resumed == 0 {
            return Err(io::Error::other("could not resume the new process"));
        }
        Ok(())
    }
}

#[cfg(unix)]
mod imp {
    use std::{io, os::unix::process::CommandExt, process::{Child, Command}};

    pub struct Tree {
        pgid: i32,
    }
    impl Tree {
        pub fn spawn(mut command: Command) -> io::Result<(Child, Tree)> {
            command.process_group(0);
            let child = command.spawn()?;
            let pgid = child.id() as i32;
            Ok((child, Tree { pgid }))
        }
        pub fn kill(&self) {
            // SAFETY: signals the process group this tree created.
            unsafe {
                libc::kill(-self.pgid, libc::SIGKILL);
            }
        }
    }
    impl Drop for Tree {
        fn drop(&mut self) {
            self.kill();
        }
    }
}

/// The owned tree of one command. Dropping it ends every process in it.
pub struct ProcessTree(imp::Tree);

impl ProcessTree {
    /// Start `command` inside a new owned tree.
    pub fn spawn(command: Command) -> std::io::Result<(Child, ProcessTree)> {
        let (child, tree) = imp::Tree::spawn(command)?;
        Ok((child, ProcessTree(tree)))
    }
    /// End every process in the tree now.
    pub fn kill(&self) {
        self.0.kill();
    }
}

/// Whether a process with this ID is still running (used by tests and by
/// restart reconciliation).
pub fn is_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, STILL_ACTIVE},
            System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
        };
        // SAFETY: the handle is checked and closed.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return false;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(handle, &mut code) != 0;
            CloseHandle(handle);
            ok && code == STILL_ACTIVE as u32
        }
    }
    #[cfg(unix)]
    {
        // A zombie still answers kill(0); check its state instead.
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => stat.rsplit(')').next().and_then(|r| r.split_whitespace().next()) != Some("Z"),
            // SAFETY: signal 0 only checks that the process exists.
            Err(_) => unsafe { libc::kill(pid as i32, 0) == 0 },
        }
    }
}

/// Linux: the process start time (clock ticks since boot), to tell a
/// process apart from a later one that reused its ID. None elsewhere.
pub fn start_marker(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Field 22, counting from after the ")" that ends the command name.
    stat.rsplit(')').next()?.split_whitespace().nth(19)?.parse().ok()
}

/// Kill a process group left over from before a restart, but only if its
/// leader is still the same process. Unix only; on Windows the Job Object
/// already ended the tree when Lu exited.
pub fn kill_leftover_group(pid: u32, marker: Option<u64>) -> bool {
    #[cfg(unix)]
    {
        if marker.is_some() && start_marker(pid) == marker && is_alive(pid) {
            // SAFETY: the group leader was verified above.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
            return true;
        }
        false
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, marker);
        false
    }
}

