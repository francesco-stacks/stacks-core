#[cfg(any(target_os = "linux", target_os = "macos"))]
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
