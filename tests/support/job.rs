//! A job object that ends a test program's whole process tree with it. Included with `#[path]`
//! by the programs tests compile for Windows (`sh_shim.rs`, the end-to-end suite's
//! `ssh_wrapper.rs`), never part of Portico.

use std::{ffi::c_void, ptr};

type Handle = *mut c_void;

#[repr(C)]
struct BasicLimits {
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

/// `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`.
#[repr(C)]
struct ExtendedLimits {
    basic: BasicLimits,
    io_counters: [u64; 6],
    process_memory_limit: usize,
    job_memory_limit: usize,
    peak_process_memory_used: usize,
    peak_job_memory_used: usize,
}

const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;

#[link(name = "kernel32")]
extern "system" {
    fn CreateJobObjectW(attributes: *mut c_void, name: *const u16) -> Handle;
    fn SetInformationJobObject(job: Handle, class: i32, info: *mut c_void, length: u32) -> i32;
    fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
    fn GetCurrentProcess() -> Handle;
}

/// Puts this process in a new job whose processes all end when its last handle closes,
/// which is when this process ends; the children it starts join the job. The handle is
/// deliberately never closed. Without a job the script still runs, only unguarded.
pub fn join_own() -> Option<Job> {
    // SAFETY: plain calls with valid arguments; `limits` is zeroed plain data of the
    // size passed, and a null job handle is checked.
    unsafe {
        let job = CreateJobObjectW(ptr::null_mut(), ptr::null());
        if job.is_null() {
            return None;
        }
        let job = Job(job);
        if job.set_limits(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE)
            && AssignProcessToJobObject(job.0, GetCurrentProcess()) != 0
        {
            Some(job)
        } else {
            None
        }
    }
}

/// The job `join_own` made.
pub struct Job(Handle);

impl Job {
    /// Lets the job's other processes outlive this one, as a Unix program's background
    /// children outlive it when it exits on its own. A program that is terminated never
    /// gets here, so its job still ends them.
    #[allow(dead_code, reason = "not every program that includes this lets go")]
    pub fn let_go(&self) {
        self.set_limits(0);
    }

    fn set_limits(&self, flags: u32) -> bool {
        // SAFETY: `limits` is zeroed plain data of the size passed, and `self.0` is a job.
        unsafe {
            let mut limits: ExtendedLimits = std::mem::zeroed();
            limits.basic.limit_flags = flags;
            SetInformationJobObject(
                self.0,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                (&mut limits as *mut ExtendedLimits).cast(),
                size_of::<ExtendedLimits>() as u32,
            ) != 0
        }
    }
}
