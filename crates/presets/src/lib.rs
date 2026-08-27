#![forbid(unsafe_code)]

use videoferry_core::{
    Container, ContentMode, Encoder, FpsPolicy, MediaInfo, MetadataPolicy, QueueSettings,
};

const DJI_CAMERA_LUTS: &[(&str, &str, &[&str])] = &[
    ("DJI OsmoAction6", "action6.cube", &["dji osmoaction6"]),
    (
        "DJI OsmoPocket3",
        "pocket3.cube",
        &["dji osmo pocket 3", "dji osmopocket3"],
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DjiCameraProfile {
    pub model_name: &'static str,
    pub lut_name: &'static str,
    pub is_dlog: bool,
}

#[must_use]
pub fn dji_camera_profiles() -> impl ExactSizeIterator<Item = DjiCameraProfile> {
    DJI_CAMERA_LUTS
        .iter()
        .map(|(model_name, lut_name, _)| DjiCameraProfile {
            model_name,
            lut_name,
            is_dlog: true,
        })
}

#[must_use]
pub fn dji_camera_profile(media: &MediaInfo) -> Option<DjiCameraProfile> {
    let encoder = media
        .metadata
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("encoder"))?
        .1
        .trim()
        .to_ascii_lowercase();

    DJI_CAMERA_LUTS
        .iter()
        .find(|(_, _, markers)| markers.iter().any(|marker| encoder.contains(marker)))
        .map(|(model_name, lut_name, _)| DjiCameraProfile {
            model_name,
            lut_name,
            // The Python behavior treats both supported models as D-Log even
            // when packet-level markers are absent.
            is_dlog: true,
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresetDescriptor {
    pub mode: ContentMode,
    pub encoder: Encoder,
    pub container: Container,
    pub share_lowest_fps: bool,
    pub ignore_existing_encoded_source: bool,
}

#[must_use]
pub fn catalog() -> Vec<PresetDescriptor> {
    let mut result = Vec::new();
    for mode in [
        ContentMode::Tv,
        ContentMode::Animation,
        ContentMode::CameraVideos,
        ContentMode::Stabilize,
        ContentMode::PhotoSlideshow,
    ] {
        for encoder in Encoder::ALL {
            result.push(descriptor(mode, encoder));
        }
    }
    result.push(descriptor(ContentMode::Trim, Encoder::X265));
    result.push(descriptor(ContentMode::Trim, Encoder::HevcNvenc));
    result
}

#[must_use]
pub fn descriptor(mode: ContentMode, encoder: Encoder) -> PresetDescriptor {
    let camera = mode == ContentMode::CameraVideos;
    PresetDescriptor {
        mode,
        encoder,
        container: if camera || mode == ContentMode::PhotoSlideshow {
            Container::Mp4
        } else {
            Container::Matroska
        },
        share_lowest_fps: matches!(mode, ContentMode::Tv | ContentMode::Animation),
        ignore_existing_encoded_source: camera,
    }
}

/// Returns the folder suffix used by the Python converter after every direct
/// media source in a directory has a matching `original/` backup.
#[must_use]
pub const fn converted_directory_suffix(
    mode: ContentMode,
    encoder: Encoder,
) -> Option<&'static str> {
    if !matches!(mode, ContentMode::Tv | ContentMode::Animation) {
        return None;
    }
    match encoder {
        Encoder::X265 => Some(" (x265)"),
        Encoder::X264 => Some(" (x264)"),
        Encoder::SvtAv1 => Some(" (libsvtav1)"),
        Encoder::HevcNvenc => None,
        Encoder::Av1Nvenc => Some(" (av1_nvenc)"),
        Encoder::H264Nvenc => Some(" (h264_nvenc)"),
        Encoder::H264VideoToolbox => Some(" (h264_videotoolbox)"),
        Encoder::HevcVideoToolbox => Some(" (hevc_videotoolbox)"),
        Encoder::Av1VideoToolbox => Some(" (av1_videotoolbox)"),
    }
}

#[must_use]
pub fn default_settings(mode: ContentMode, encoder: Encoder) -> QueueSettings {
    let mut settings = QueueSettings {
        mode,
        encoder,
        fps: if matches!(mode, ContentMode::Tv | ContentMode::Animation) {
            FpsPolicy::SharedLowest
        } else {
            FpsPolicy::Source
        },
        metadata: if matches!(
            mode,
            ContentMode::CameraVideos | ContentMode::Stabilize | ContentMode::Trim
        ) {
            MetadataPolicy::Preserve
        } else {
            MetadataPolicy::Remove
        },
        apply_lut: mode == ContentMode::CameraVideos && !encoder.is_hardware(),
        ..QueueSettings::default()
    };

    match encoder {
        Encoder::X265 => {
            settings.quality = Some(
                if matches!(
                    mode,
                    ContentMode::CameraVideos
                        | ContentMode::Stabilize
                        | ContentMode::PhotoSlideshow
                ) {
                    18.0
                } else {
                    28.0
                },
            );
            settings.speed_preset = Some("medium".to_owned());
        }
        Encoder::X264 => {
            settings.quality = Some(23.0);
            settings.speed_preset = Some("medium".to_owned());
        }
        Encoder::SvtAv1 => {
            settings.quality = Some(
                if matches!(
                    mode,
                    ContentMode::CameraVideos | ContentMode::PhotoSlideshow
                ) {
                    24.0
                } else {
                    35.0
                },
            );
            settings.speed_preset = Some("6".to_owned());
        }
        Encoder::HevcNvenc | Encoder::Av1Nvenc | Encoder::H264Nvenc => {
            settings.quality = None;
            settings.speed_preset = Some("p4".to_owned());
        }
        Encoder::H264VideoToolbox | Encoder::HevcVideoToolbox | Encoder::Av1VideoToolbox => {
            settings.quality = None;
            settings.speed_preset = None;
        }
    }
    if mode == ContentMode::Trim {
        settings.trim_start = Some(std::time::Duration::ZERO);
        settings.trim_end = Some(std::time::Duration::from_secs(10));
    }
    settings
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::{
        catalog, converted_directory_suffix, default_settings, dji_camera_profile,
        dji_camera_profiles,
    };
    use videoferry_core::{ContentMode, Encoder, FpsPolicy, MediaInfo, MetadataPolicy};

    #[test]
    fn catalog_matches_current_mode_encoder_matrix() {
        assert_eq!(catalog().len(), 47);
    }

    #[test]
    fn camera_defaults_preserve_source_fps() {
        let settings = default_settings(ContentMode::CameraVideos, Encoder::X265);
        assert_eq!(settings.fps, FpsPolicy::Source);
        assert_eq!(settings.quality, Some(18.0));
        assert_eq!(settings.metadata, MetadataPolicy::Preserve);
        assert!(settings.apply_lut);
        assert!(!default_settings(ContentMode::CameraVideos, Encoder::HevcNvenc).apply_lut);
    }

    #[test]
    fn specialized_workflow_defaults_match_the_python_dialog() {
        assert_eq!(
            default_settings(ContentMode::PhotoSlideshow, Encoder::X265).quality,
            Some(18.0)
        );
        assert_eq!(
            default_settings(ContentMode::PhotoSlideshow, Encoder::SvtAv1).quality,
            Some(24.0)
        );
        assert_eq!(
            default_settings(ContentMode::Stabilize, Encoder::X265).quality,
            Some(18.0)
        );

        let trim = default_settings(ContentMode::Trim, Encoder::X265);
        assert_eq!(trim.trim_start, Some(std::time::Duration::ZERO));
        assert_eq!(trim.trim_end, Some(std::time::Duration::from_secs(10)));
    }

    #[test]
    fn nvidia_encoders_default_to_the_medium_p4_preset() {
        for encoder in [Encoder::H264Nvenc, Encoder::HevcNvenc, Encoder::Av1Nvenc] {
            assert_eq!(
                default_settings(ContentMode::Tv, encoder)
                    .speed_preset
                    .as_deref(),
                Some("p4")
            );
        }
    }

    #[test]
    fn detects_the_current_dji_lut_markers_case_insensitively() {
        let mut metadata = BTreeMap::new();
        metadata.insert("ENCODER".to_owned(), "DJI Osmo Pocket 3".to_owned());
        let media = MediaInfo {
            path: PathBuf::from("camera.mp4"),
            container_name: "mov,mp4".to_owned(),
            duration: None,
            file_size: None,
            bit_rate: None,
            width: None,
            height: None,
            frame_rate: None,
            streams: Vec::new(),
            metadata,
        };

        let profile = dji_camera_profile(&media).expect("DJI profile");
        assert_eq!(profile.model_name, "DJI OsmoPocket3");
        assert_eq!(profile.lut_name, "pocket3.cube");
        assert!(profile.is_dlog);
    }

    #[test]
    fn published_dji_lut_map_matches_the_python_settings_dialog() {
        let profiles = dji_camera_profiles()
            .map(|profile| (profile.model_name, profile.lut_name))
            .collect::<Vec<_>>();
        assert_eq!(
            profiles,
            [
                ("DJI OsmoAction6", "action6.cube"),
                ("DJI OsmoPocket3", "pocket3.cube"),
            ]
        );
    }

    #[test]
    fn completed_folder_suffixes_match_python_converter_classes() {
        assert_eq!(
            converted_directory_suffix(ContentMode::Tv, Encoder::X265),
            Some(" (x265)")
        );
        assert_eq!(
            converted_directory_suffix(ContentMode::Animation, Encoder::SvtAv1),
            Some(" (libsvtav1)")
        );
        assert_eq!(
            converted_directory_suffix(ContentMode::Tv, Encoder::HevcNvenc),
            None
        );
        assert_eq!(
            converted_directory_suffix(ContentMode::CameraVideos, Encoder::X265),
            None
        );
    }
}
