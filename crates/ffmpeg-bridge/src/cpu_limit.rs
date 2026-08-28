use std::io;

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
