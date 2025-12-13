#[cfg(target_os = "macos")]
mod darwin {
    unsafe extern "C" {
        // In libSystem on macOS:
        // uint64_t clock_gettime_nsec_np(clockid_t clk_id);
        pub fn clock_gettime_nsec_np(clk_id: libc::clockid_t) -> u64;
    }
}

#[cfg(target_os = "macos")]
#[inline(always)]
pub fn thread_cpu_nanos() -> u64 {
    unsafe { darwin::clock_gettime_nsec_np(libc::CLOCK_THREAD_CPUTIME_ID) }
}

#[cfg(any(target_os = "linux"))]
#[inline(always)]
pub fn thread_cpu_nanos() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts);
    }
    (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[inline(always)]
pub fn thread_cpu_nanos() -> u64 {
    0
}

/// Sanity-check test to ensure that on macOS, both available methods of
/// reading the thread CPU timer yield consistent results.
#[cfg(target_os = "macos")]
#[test]
fn macos_thread_cpu_timer_equivalence_smoke() {
    // Method 1: via timespec (available on linux & macos)
    fn via_timespec() -> u64 {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let _ = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
        (ts.tv_sec as u64) * 1_000_000_000u64 + (ts.tv_nsec as u64)
    }

    // Method 2: via clock_gettime_nsec_np (macOS/darwin-specific)
    fn via_nsec_np() -> u64 {
        unsafe { darwin::clock_gettime_nsec_np(libc::CLOCK_THREAD_CPUTIME_ID) }
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
