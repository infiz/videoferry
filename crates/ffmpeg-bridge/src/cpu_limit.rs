use std::io;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
struct ProcessCpuSample {
    wall_time: Instant,
    process_time: Duration,
}

/// Samples this process's CPU use as a percentage of the whole machine.
#[derive(Debug)]
pub struct ProcessCpuSampler {
    logical_processors: usize,
    previous: Option<ProcessCpuSample>,
}

impl ProcessCpuSampler {
    #[must_use]
    pub fn new() -> Self {
        Self {
            logical_processors: std::thread::available_parallelism()
                .map_or(1, std::num::NonZero::get),
            previous: None,
        }
    }

    /// Returns CPU use normalized to 0–100% across all logical processors.
    ///
    /// The first sample establishes a baseline and returns `None`.
    pub fn sample_percent(&mut self) -> Option<f64> {
        let current = ProcessCpuSample {
            wall_time: Instant::now(),
            process_time: process_cpu_time()?,
        };
        let previous = self.previous.replace(current)?;
        normalized_process_cpu_percent(
            current.process_time.saturating_sub(previous.process_time),
            current
                .wall_time
                .saturating_duration_since(previous.wall_time),
            self.logical_processors,
        )
    }

    pub fn reset(&mut self) {
        self.previous = None;
    }
}

impl Default for ProcessCpuSampler {
    fn default() -> Self {
        Self::new()
    }
}

fn normalized_process_cpu_percent(
    process_delta: Duration,
    wall_delta: Duration,
    logical_processors: usize,
) -> Option<f64> {
    if wall_delta.is_zero() || logical_processors == 0 {
        return None;
    }
    let logical_processors = u32::try_from(logical_processors).ok()?;
    Some(
        (process_delta.as_secs_f64() / wall_delta.as_secs_f64() / f64::from(logical_processors)
            * 100.0)
            .clamp(0.0, 100.0),
    )
}

#[cfg(target_os = "windows")]
fn process_cpu_time() -> Option<Duration> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: GetCurrentProcess returns a valid pseudo-handle, and all FILETIME
    // pointers refer to initialized writable values for the duration of the call.
    let succeeded = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &raw mut creation,
            &raw mut exit,
            &raw mut kernel,
            &raw mut user,
        )
    };
    if succeeded == 0 {
        return None;
    }
    let ticks = file_time_ticks(kernel).checked_add(file_time_ticks(user))?;
    Some(Duration::from_nanos(ticks.checked_mul(100)?))
}

#[cfg(target_os = "windows")]
fn file_time_ticks(value: windows_sys::Win32::Foundation::FILETIME) -> u64 {
    (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime)
}

#[cfg(unix)]
fn process_cpu_time() -> Option<Duration> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `usage` points to sufficient writable storage for getrusage, and
    // a successful call initializes the complete rusage value.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: getrusage returned success and initialized `usage` above.
    let usage = unsafe { usage.assume_init() };
    let user = timeval_microseconds(usage.ru_utime)?;
    let system = timeval_microseconds(usage.ru_stime)?;
    Some(Duration::from_micros(user.checked_add(system)?))
}

#[cfg(unix)]
fn timeval_microseconds(value: libc::timeval) -> Option<u64> {
    let seconds = u64::try_from(value.tv_sec).ok()?;
    let microseconds = u64::try_from(value.tv_usec).ok()?;
    seconds
        .checked_mul(1_000_000)
        .and_then(|total| total.checked_add(microseconds))
}

#[cfg(not(any(target_os = "windows", unix)))]
const fn process_cpu_time() -> Option<Duration> {
    None
}

/// Limits the logical processors available to the current `VideoFerry` process.
///
/// On Windows this updates the process affinity mask, which immediately affects
/// an active native encoder and every conversion started afterward. Other
/// platforms keep the same safe API while `FFmpeg`'s per-file thread limit remains
/// the enforcement mechanism.
#[derive(Debug)]
pub struct ProcessCpuLimiter {
    available_threads: usize,
    #[cfg(target_os = "windows")]
    original_affinity: Option<usize>,
}

impl ProcessCpuLimiter {
    #[must_use]
    pub fn new() -> Self {
        let fallback = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        #[cfg(target_os = "windows")]
        {
            let original_affinity = process_affinity().ok();
            let available_threads = original_affinity
                .map_or(fallback, |mask| mask.count_ones() as usize)
                .max(1);
            Self {
                available_threads,
                original_affinity,
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            Self {
                available_threads: fallback,
            }
        }
    }

    #[must_use]
    pub const fn available_threads(&self) -> usize {
        self.available_threads
    }

    /// Applies a process-wide logical-processor limit when the platform supports it.
    ///
    /// # Errors
    ///
    /// Returns the operating-system error if Windows cannot read or update the
    /// current process affinity.
    pub fn set_thread_limit(&mut self, threads: usize) -> io::Result<()> {
        let threads = threads.clamp(1, self.available_threads);
        #[cfg(target_os = "windows")]
        {
            let original = self
                .original_affinity
                .ok_or_else(io::Error::last_os_error)?;
            set_process_affinity(first_processors(original, threads))
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = threads;
            Ok(())
        }
    }
}

impl Default for ProcessCpuLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "windows")]
fn process_affinity() -> io::Result<usize> {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessAffinityMask};

    let mut process_mask = 0_usize;
    let mut system_mask = 0_usize;
    // SAFETY: GetCurrentProcess returns a pseudo-handle valid in this process,
    // and both mask pointers refer to initialized writable values for the call.
    let succeeded = unsafe {
        GetProcessAffinityMask(
            GetCurrentProcess(),
            &raw mut process_mask,
            &raw mut system_mask,
        )
    };
    if succeeded == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(process_mask)
    }
}

#[cfg(target_os = "windows")]
fn set_process_affinity(mask: usize) -> io::Result<()> {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, SetProcessAffinityMask};

    // SAFETY: GetCurrentProcess returns a valid pseudo-handle, and `mask` is a
    // non-empty subset of the affinity mask previously returned by Windows.
    if unsafe { SetProcessAffinityMask(GetCurrentProcess(), mask) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(target_os = "windows")]
fn first_processors(available: usize, count: usize) -> usize {
    let mut remaining = count;
    let mut selected = 0_usize;
    for bit in 0..usize::BITS {
        let candidate = 1_usize << bit;
        if available & candidate != 0 {
            selected |= candidate;
            remaining -= 1;
            if remaining == 0 {
                break;
            }
        }
    }
    selected
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    #[test]
    fn cpu_usage_is_normalized_across_logical_processors() {
        let usage = super::normalized_process_cpu_percent(
            Duration::from_secs(20),
            Duration::from_secs(1),
            32,
        )
        .unwrap();

        assert!((usage - 62.5).abs() < f64::EPSILON);
    }

    #[test]
    fn cpu_usage_is_bounded_and_rejects_an_empty_interval() {
        assert_eq!(
            super::normalized_process_cpu_percent(
                Duration::from_secs(64),
                Duration::from_secs(1),
                32,
            ),
            Some(100.0)
        );
        assert_eq!(
            super::normalized_process_cpu_percent(Duration::ZERO, Duration::ZERO, 32),
            None
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn affinity_selection_uses_only_available_processors() {
        assert_eq!(super::first_processors(0b1011_0100, 1), 0b0000_0100);
        assert_eq!(super::first_processors(0b1011_0100, 3), 0b0011_0100);
        assert_eq!(super::first_processors(0b1011_0100, 4), 0b1011_0100);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn maximum_limit_preserves_the_available_affinity() {
        let mut limiter = super::ProcessCpuLimiter::new();
        limiter
            .set_thread_limit(limiter.available_threads())
            .unwrap();
    }
}
