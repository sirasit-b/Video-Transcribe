//! Media probing and auto-editor's timebase choice.

use std::path::Path;
use std::process::Command;

use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Rational {
    pub num: i64,
    pub den: i64,
}

impl Rational {
    pub fn as_f64(self) -> f64 {
        self.num as f64 / self.den as f64
    }

    /// auto-editor's `isValid`: a rational with a zero on either side carries no
    /// frame rate at all (`0/0` is what FFmpeg reports when none is declared).
    fn is_valid(self) -> bool {
        self.num != 0 && self.den != 0
    }

    fn parse(text: &str) -> Option<Self> {
        let (num, den) = text.split_once('/')?;
        Some(Rational {
            num: num.trim().parse().ok()?,
            den: den.trim().parse().ok()?,
        })
    }
}

impl std::fmt::Display for Rational {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.num, self.den)
    }
}

#[derive(Clone, Debug)]
pub struct AudioStream {
    /// Index among the audio streams only — what `-map 0:a:N` takes.
    pub ordinal: usize,
    pub sample_rate: u32,
    pub channels: usize,
}

#[derive(Clone, Debug)]
pub struct MediaInfo {
    pub has_video: bool,
    pub frame_rate: Rational,
    /// Picture size, for estimating how long an encode will take.
    pub width: u32,
    pub height: u32,
    pub audio: Vec<AudioStream>,
    pub duration: f64,
}

pub fn probe(ffprobe: &str, path: &Path) -> Result<MediaInfo, String> {
    let output = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path)
        .output()
        .map_err(|err| format!("failed to run ffprobe: {err}"))?;

    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }

    let parsed: Value = serde_json::from_slice(&output.stdout)
        .map_err(|err| format!("could not parse ffprobe output: {err}"))?;

    let mut has_video = false;
    let mut frame_rate = Rational { num: 0, den: 0 };
    let mut width = 0u32;
    let mut height = 0u32;
    let mut audio: Vec<AudioStream> = Vec::new();

    for stream in parsed["streams"].as_array().unwrap_or(&Vec::new()) {
        match stream["codec_type"].as_str().unwrap_or_default() {
            "video" => {
                // Cover art and other single-frame "video" streams are not a
                // picture track; treating one as the timebase source would put
                // the whole timeline on a 1-frame clock.
                if stream["disposition"]["attached_pic"].as_i64().unwrap_or(0) == 1 {
                    continue;
                }
                if has_video {
                    continue;
                }
                has_video = true;
                width = stream["width"].as_u64().unwrap_or(0) as u32;
                height = stream["height"].as_u64().unwrap_or(0) as u32;
                frame_rate = stream["avg_frame_rate"]
                    .as_str()
                    .and_then(Rational::parse)
                    .filter(|r| r.is_valid())
                    .or_else(|| {
                        stream["r_frame_rate"]
                            .as_str()
                            .and_then(Rational::parse)
                            .filter(|r| r.is_valid())
                    })
                    .unwrap_or(Rational { num: 0, den: 0 });
            }
            "audio" => {
                let ordinal = audio.len();
                let sample_rate = stream["sample_rate"]
                    .as_str()
                    .and_then(|value| value.parse::<u32>().ok())
                    .unwrap_or(0);
                let channels = stream["channels"].as_u64().unwrap_or(0) as usize;
                audio.push(AudioStream {
                    ordinal,
                    sample_rate,
                    channels,
                });
            }
            _ => {}
        }
    }

    let duration = parsed["format"]["duration"]
        .as_str()
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(0.0);

    Ok(MediaInfo {
        has_video,
        frame_rate,
        width,
        height,
        audio,
        duration,
    })
}

fn gcd(a: i64, b: i64) -> i64 {
    let (mut a, mut b) = (a.abs(), b.abs());
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a.max(1)
}

/// Port of auto-editor's `makeSaneTimebase` (src/timeline.nim): the timeline runs
/// at the source's frame rate, rounded to two decimals, with the three NTSC rates
/// restored to their exact rationals.
pub fn sane_timebase(frame_rate: Rational) -> Rational {
    if !frame_rate.is_valid() {
        return Rational { num: 30, den: 1 };
    }

    // Comparing hundredths as integers is the same test as Nim's
    // `round(tb.float64, 2) == round(ntsc.float64, 2)`, without the float noise.
    let hundredths = (frame_rate.as_f64() * 100.0).round() as i64;
    match hundredths {
        5994 => return Rational { num: 60000, den: 1001 },
        2997 => return Rational { num: 30000, den: 1001 },
        2398 => return Rational { num: 24000, den: 1001 },
        _ => {}
    }

    if hundredths <= 0 {
        return Rational { num: 30, den: 1 };
    }

    let divisor = gcd(hundredths, 100);
    Rational {
        num: hundredths / divisor,
        den: 100 / divisor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ntsc_rates_keep_their_exact_rationals() {
        assert_eq!(
            sane_timebase(Rational { num: 30000, den: 1001 }),
            Rational { num: 30000, den: 1001 }
        );
        assert_eq!(
            sane_timebase(Rational { num: 24000, den: 1001 }),
            Rational { num: 24000, den: 1001 }
        );
        assert_eq!(
            sane_timebase(Rational { num: 60000, den: 1001 }),
            Rational { num: 60000, den: 1001 }
        );
    }

    #[test]
    fn whole_rates_reduce_to_themselves() {
        assert_eq!(sane_timebase(Rational { num: 30, den: 1 }), Rational { num: 30, den: 1 });
        assert_eq!(sane_timebase(Rational { num: 50, den: 1 }), Rational { num: 50, den: 1 });
        assert_eq!(
            sane_timebase(Rational { num: 1000, den: 40 }),
            Rational { num: 25, den: 1 }
        );
    }

    #[test]
    fn an_odd_rate_snaps_to_two_decimals() {
        // 14.985 fps -> 14.99 -> 1499/100
        assert_eq!(
            sane_timebase(Rational { num: 15000, den: 1001 }),
            Rational { num: 1499, den: 100 }
        );
    }

    #[test]
    fn a_missing_frame_rate_falls_back_to_30() {
        assert_eq!(sane_timebase(Rational { num: 0, den: 0 }), Rational { num: 30, den: 1 });
    }
}
