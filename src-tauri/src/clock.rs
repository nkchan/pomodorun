pub fn utc_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
#[cfg(target_os = "macos")]
pub fn continuous_ms() -> u64 {
    #[repr(C)]
    struct Timebase {
        numer: u32,
        denom: u32,
    }
    unsafe extern "C" {
        fn mach_continuous_time() -> u64;
        fn mach_timebase_info(info: *mut Timebase) -> i32;
    }
    static RATIO: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();
    let (numer, denom) = *RATIO.get_or_init(|| {
        let mut t = Timebase { numer: 0, denom: 0 };
        unsafe {
            mach_timebase_info(&mut t);
        }
        (t.numer, t.denom)
    });
    // u128 prevents overflow of ticks * numerator after long uptimes.
    (u128::from(unsafe { mach_continuous_time() }) * u128::from(numer)
        / u128::from(denom)
        / 1_000_000) as u64
}
#[cfg(not(target_os = "macos"))]
pub fn continuous_ms() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
}
