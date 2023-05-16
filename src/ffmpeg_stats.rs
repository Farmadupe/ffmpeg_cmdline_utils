use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::*;

#[derive(Debug, Deserialize, Serialize, Clone, Error)]
pub enum VideoInfoError {
    #[error("Error parsing stats: {0}")]
    JsonError(String),
    #[error("Error parsing stats: {0}")]
    ParseIntError(String),
    #[error("Error parsing stats: {0}")]
    ParseFloatError(String),
}

impl From<serde_json::Error> for VideoInfoError {
    fn from(e: serde_json::Error) -> Self {
        VideoInfoError::JsonError(format!("{}", e))
    }
}

impl From<std::num::ParseIntError> for VideoInfoError {
    fn from(e: std::num::ParseIntError) -> Self {
        VideoInfoError::ParseIntError(format!("{}", e))
    }
}

impl From<std::num::ParseFloatError> for VideoInfoError {
    fn from(e: std::num::ParseFloatError) -> Self {
        VideoInfoError::ParseFloatError(format!("{}", e))
    }
}

// There is a slighty gotcha in ffmpeg where if the video metadata declares a rotation,
// raw (x, y) resolution in that metadata refers to the "unrotated" resolution. we must
// therefore swap the x and y values if the rotation is 90 or 270
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Debug, Copy, Serialize, Deserialize, Hash)]
enum FfmpegVideoRotation {
    Rot0,
    Rot90,
    Rot180,
    Rot270,
}
use FfmpegVideoRotation::*;

impl Default for FfmpegVideoRotation {
    fn default() -> Self {
        Self::Rot0
    }
}

#[derive(PartialEq, Clone, Debug, Serialize, Deserialize, Default)]
pub struct VideoInfo {
    pub duration: f64,
    pub size: u64,
    pub bit_rate: u32,
    pub resolution: (u32, u32),
    pub has_audio: bool,
}

impl VideoInfo {
    pub fn new<P>(src_path: P) -> Result<Self, FfmpegErrorKind>
    where
        P: AsRef<Path>,
    {
        use serde_json::Value;

        let stats_string = get_video_stats(&src_path)?;
        let stats_parsed: Value = serde_json::from_str(&stats_string).map_err(VideoInfoError::from)?;

        let duration = &stats_parsed["format"]["duration"];
        let duration = if let Value::String(d) = duration {
            d.parse().map_err(VideoInfoError::from)?
        } else {
            0.0
        };

        let size = &stats_parsed["format"]["size"];
        let size = if let Value::String(s) = size {
            s.parse().map_err(VideoInfoError::from)?
        } else {
            0
        };

        let bit_rate = &stats_parsed["format"]["bit_rate"];
        let bit_rate = if let Value::String(br) = bit_rate {
            br.parse().map_err(VideoInfoError::from)?
        } else {
            0
        };

        fn streams_of_type(stats_parsed: &Value, stream_type: &str) -> Option<Vec<Value>> {
            if let Value::Array(streams) = &stats_parsed["streams"] {
                let ret = streams
                    .iter()
                    .filter(|s| match &s["codec_type"] {
                        Value::String(codec_type) => codec_type == stream_type,
                        _ => false,
                    })
                    .cloned()
                    .collect();

                Some(ret)
            } else {
                None
            }
        }

        fn first_u32_from_video_streams(stats_parsed: &Value, field_name: &str) -> Option<u32> {
            let video_streams = streams_of_type(stats_parsed, "video")?;

            let all_matched_values = video_streams
                .iter()
                .filter_map(|stream| {
                    if let Value::Number(v) = &stream[field_name] {
                        Some(v.as_u64()? as u32)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();

            all_matched_values.iter().cloned().next()
        }

        // If the video metadata declares that a video is rotated, then FFMPEG will conveniently autorotate
        // each frame for us, however we will have to remember to swap around x and y axis if the rotation is
        // 90 or 270
        let error_out = |rotation: &str| -> Result<FfmpegVideoRotation, VideoInfoError> {
            Err(VideoInfoError::ParseIntError(format!(
                "ffprobe failure. Got unexpected rotation. src_path: {}, {rotation}",
                src_path.as_ref().display()
            )))
        };

        let rotation = {
            match streams_of_type(&stats_parsed, "video") {
                Some(video_streams) => match video_streams.as_slice() {
                    [] => Ok(FfmpegVideoRotation::Rot0),
                    _ => match &video_streams[0]["side_data_list"][0]["rotation"] {
                        //rotation not found (most videos)
                        Value::Null => Ok(FfmpegVideoRotation::Rot0),

                        //rotation found
                        Value::String(rotation) => {
                            let rotation = rotation.parse::<i64>();
                            match rotation {
                                Ok(0) => Ok(FfmpegVideoRotation::Rot0),
                                Ok(90) => Ok(FfmpegVideoRotation::Rot90),
                                Ok(180) => Ok(FfmpegVideoRotation::Rot180),
                                Ok(-90) | Ok(270) => Ok(FfmpegVideoRotation::Rot270),
                                Ok(bad_rot) => error_out(bad_rot.to_string().as_str()),
                                Err(e) => Err(VideoInfoError::from(e)),
                            }
                        }

                        //rotation found
                        Value::Number(rotation) => {
                            let rotation = rotation.as_i64();
                            match rotation {
                                Some(0) => Ok(FfmpegVideoRotation::Rot0),
                                Some(90) => Ok(FfmpegVideoRotation::Rot90),
                                Some(180) | Some(-180) => Ok(FfmpegVideoRotation::Rot180),
                                Some(-90) | Some(270) => Ok(FfmpegVideoRotation::Rot270),
                                Some(bad_rot) => error_out(bad_rot.to_string().as_str()),
                                None => error_out("<ERROR>"),
                            }
                        }

                        _ => Err(VideoInfoError::JsonError(
                            "Failed to parse JSON".to_string(),
                        )),
                    },
                },
                None => Ok(FfmpegVideoRotation::Rot0),
            }
        }?;
        //println!("{:#?}: {}", rotation, src_path.as_ref().display());

        let first_width = first_u32_from_video_streams(&stats_parsed, "width").unwrap_or(0);
        let first_height = first_u32_from_video_streams(&stats_parsed, "height").unwrap_or(0);

        let resolution = if matches!(rotation, Rot0 | Rot180) {
            (first_width, first_height)
        } else {
            (first_height, first_width)
        };

        let audio_streams = streams_of_type(&stats_parsed, "audio");
        let has_audio = match audio_streams {
            None => false,
            Some(audio_streams) => audio_streams.iter().any(|stream| match &stream["codec_type"] {
                    Value::String(codec_type) => codec_type == "audio",
                    _ => false,
                }),
        };

        Ok(VideoInfo {
            duration,
            size,
            bit_rate,
            resolution,
            has_audio,
        })
    }

    pub fn duration(&self) -> f64 {
        self.duration
    }
    pub fn size(&self) -> u64 {
        self.size
    }
    pub fn bit_rate(&self) -> u32 {
        self.bit_rate
    }
    pub fn resolution(&self) -> (u32, u32) {
        self.resolution
    }
    pub fn has_audio(&self) -> bool {
        self.has_audio
    }
}
