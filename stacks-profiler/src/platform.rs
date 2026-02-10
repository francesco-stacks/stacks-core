//! Platform-specific per-thread CPU time.
//!
//! Each platform module implements [`ThreadCpuTimer`] on a zero-sized struct,
//! and a single `cfg`-gated `use` selects the active implementation.  The
//! public [`thread_cpu_nanos`] free function delegates to whichever platform
//! was selected at compile time.
//!
//! Returns the cumulative CPU time (user + kernel) consumed by the calling
//! thread, in nanoseconds.
//!
//! | Platform | Source | Typical resolution |
//! |----------|--------|--------------------|
//! | macOS | `clock_gettime_nsec_np(CLOCK_THREAD_CPUTIME_ID)` | sub-microsecond |
//! | Linux | `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` | sub-microsecond |
//! | Windows | `GetThreadTimes` (kernel32) | ~15.6 ms (system clock tick) |
//! | Other | — | returns 0 (unsupported) |
//!
//! # Windows caveats
//!
//! `GetThreadTimes` reports CPU time in `FILETIME` units (100 ns intervals),
//! but the underlying counter only advances once per system clock interrupt,
//! which defaults to ~15.625 ms (64 Hz). This means:
//!
//! - Individual short spans may report **0 ns** of CPU time.
//! - Aggregated totals across many calls converge to accurate values.
//! - Wall-time minus CPU-time ("wait time") is unreliable for sub-16 ms spans.
//!
//! The alternative `QueryThreadCycleTime` offers cycle-level precision but
//! returns CPU cycles rather than wall-clock nanoseconds; converting back
//! requires knowledge of the effective clock frequency, which varies under
//! dynamic frequency scaling. `GetThreadTimes` was chosen for its direct
//! time-unit semantics and simplicity.

// ── trait ────────────────────────────────────────────────────────────────────

/// Contract that every platform backend must satisfy.
///
/// Implementations live on zero-sized structs so the compiler can
/// monomorphise and inline the call — no vtable overhead.
trait ThreadCpuTimer {
    /// Cumulative CPU time (user + kernel) of the calling thread, in
    /// nanoseconds.  Must be monotonically non-decreasing within a
    /// thread.
    fn thread_cpu_nanos() -> u64;
}

// ── MacOS ────────────────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
mod darwin {
    unsafe extern "C" {
        // In libSystem on macOS:
        // uint64_t clock_gettime_nsec_np(clockid_t clk_id);
        pub(super) fn clock_gettime_nsec_np(clk_id: libc::clockid_t) -> u64;
    }

    pub(super) struct Timer;

    impl super::ThreadCpuTimer for Timer {
        #[inline(always)]
        fn thread_cpu_nanos() -> u64 {
            // Sub-microsecond resolution; single FFI call.
            unsafe { clock_gettime_nsec_np(libc::CLOCK_THREAD_CPUTIME_ID) }
        }
    }

    /// Sanity-check that both available macOS methods of reading the
    /// thread CPU timer yield consistent results.
    #[test]
    fn timer_equivalence_smoke() {
        fn via_timespec() -> u64 {
            let mut ts = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            let _ = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
            (ts.tv_sec as u64) * 1_000_000_000u64 + (ts.tv_nsec as u64)
        }

        fn via_nsec_np() -> u64 {
            unsafe { clock_gettime_nsec_np(libc::CLOCK_THREAD_CPUTIME_ID) }
        }

        const EPS_NS: u64 = 50_000;

        for _ in 0..10_000 {
            let a1 = via_timespec();
            let b = via_nsec_np();
            let a2 = via_timespec();

            assert!(
                a2 >= a1,
                "timespec clock was not monotonic: a1={a1}, a2={a2}"
            );
            if b + EPS_NS < a1 || b > a2 + EPS_NS {
                panic!("nsec_np not consistent: a1={a1}, b={b}, a2={a2}, eps={EPS_NS}ns");
            }
        }
    }
}

// ── Linux ────────────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
mod linux {
    pub(super) struct Timer;

    impl super::ThreadCpuTimer for Timer {
        #[inline(always)]
        fn thread_cpu_nanos() -> u64 {
            // Sub-microsecond resolution via POSIX clock_gettime.
            let mut ts = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            unsafe {
                libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts);
            }
            (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
        }
    }
}

// ── Windows ──────────────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
mod windows {
    /// Win32 FILETIME — two 32-bit parts forming a 64-bit count of
    /// 100-nanosecond intervals since 1601-01-01 UTC.
    #[repr(C)]
    struct FILETIME {
        low: u32,
        high: u32,
    }

    impl FILETIME {
        /// Convert to a single u64 (units: 100 ns intervals).
        #[inline]
        fn as_100ns(&self) -> u64 {
            (self.high as u64) << 32 | self.low as u64
        }
    }

    #[allow(non_snake_case)] // Windows FFI uses PascalCase
    unsafe extern "system" {
        /// Returns a pseudo-handle for the calling thread (no need to close).
        fn GetCurrentThread() -> *mut core::ffi::c_void;

        /// Retrieves timing information for the specified thread.
        /// <https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-getthreadtimes>
        fn GetThreadTimes(
            hThread: *mut core::ffi::c_void,
            lpCreationTime: *mut FILETIME,
            lpExitTime: *mut FILETIME,
            lpKernelTime: *mut FILETIME,
            lpUserTime: *mut FILETIME,
        ) -> i32;
    }

    pub(super) struct Timer;

    impl super::ThreadCpuTimer for Timer {
        #[inline(always)]
        fn thread_cpu_nanos() -> u64 {
            // See module-level docs for resolution caveats (~15.6 ms).
            unsafe {
                let mut creation: FILETIME = core::mem::zeroed();
                let mut exit: FILETIME = core::mem::zeroed();
                let mut kernel: FILETIME = core::mem::zeroed();
                let mut user: FILETIME = core::mem::zeroed();

                let handle = GetCurrentThread();
                if GetThreadTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) != 0 {
                    // kernel + user = total CPU time.
                    // FILETIME units are 100 ns intervals; multiply by 100 for nanos.
                    (kernel.as_100ns() + user.as_100ns()) * 100
                } else {
                    0
                }
            }
        }
    }
}

// ── Unsupported ──────────────────────────────────────────────────────────────

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod unsupported {
    pub(super) struct Timer;

    impl super::ThreadCpuTimer for Timer {
        #[inline(always)]
        fn thread_cpu_nanos() -> u64 {
            // No per-thread CPU timer available on this platform.
            0
        }
    }
}

// ── Platform selection ───────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
use darwin::Timer as PlatformTimer;
#[cfg(target_os = "linux")]
use linux::Timer as PlatformTimer;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
use unsupported::Timer as PlatformTimer;
#[cfg(target_os = "windows")]
use windows::Timer as PlatformTimer;

/// Returns the cumulative CPU time (user + kernel) of the calling thread
/// in nanoseconds.  See [module-level docs](self) for per-platform
/// resolution and caveats.
#[inline(always)]
pub fn thread_cpu_nanos() -> u64 {
    PlatformTimer::thread_cpu_nanos()
}
