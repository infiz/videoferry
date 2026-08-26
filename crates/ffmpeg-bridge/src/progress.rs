use std::time::Duration;

use videoferry_core::ProgressRatio;

#[derive(Clone, Copy)]
pub(crate) struct ProgressMetadata {
    pub(crate) total: Option<Duration>,
    pub(crate) total_frames: Option<u64>,
    pub(crate) target_fps: Option<f64>,
}

#[derive(Clone, Copy)]
pub(crate) enum ProgressPhase {
    FirstHalf,
    SecondHalf,
}

pub(crate) fn phase_progress(
    metadata: ProgressMetadata,
    frames: u64,
    media_time: Duration,
    phase: ProgressPhase,
) -> Option<ProgressRatio> {
    let (completed, total) = metadata
        .total_frames
        .filter(|total| *total > 0)
        .map(|total| (u128::from(frames.min(total)), u128::from(total)))
        .or_else(|| {
            metadata
                .total
                .filter(|total| !total.is_zero())
                .map(|total| (media_time.min(total).as_nanos(), total.as_nanos()))
        })?;
    let completed = match phase {
        ProgressPhase::FirstHalf => completed,
        ProgressPhase::SecondHalf => total.saturating_add(completed),
    };
    Some(ProgressRatio {
        completed,
        total: total.saturating_mul(2),
    })
}

#[cfg(test)]
mod tests {
    use super::{ProgressMetadata, ProgressPhase, phase_progress};
    use std::time::Duration;

    #[test]
    fn two_pass_progress_offsets_only_the_overall_ratio() {
        let metadata = ProgressMetadata {
            total: Some(Duration::from_secs(100)),
            total_frames: Some(100),
            target_fps: Some(1.0),
        };

        let first = phase_progress(
            metadata,
            25,
            Duration::from_secs(25),
            ProgressPhase::FirstHalf,
        )
        .unwrap();
        assert_eq!((first.completed, first.total), (25, 200));

        let second = phase_progress(
            metadata,
            25,
            Duration::from_secs(25),
            ProgressPhase::SecondHalf,
        )
        .unwrap();
        assert_eq!((second.completed, second.total), (125, 200));
    }

    #[test]
    fn two_pass_progress_falls_back_to_media_time() {
        let metadata = ProgressMetadata {
            total: Some(Duration::from_secs(80)),
            total_frames: None,
            target_fps: None,
        };
        let second = phase_progress(
            metadata,
            0,
            Duration::from_secs(20),
            ProgressPhase::SecondHalf,
        )
        .unwrap();
        assert_eq!(second.completed * 8, second.total * 5);
    }
}
