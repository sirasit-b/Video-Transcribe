//! Bringing several recordings of one moment to the same level.
//!
//! A lapel microphone on the person and a laptop capturing its own screen hear the
//! same room at wildly different levels — thirty decibels apart is ordinary. That
//! difference does not trouble the lining-up, which reads the shape of the sound
//! and not its size, but it destroys the cut: the edit keeps a frame that is louder
//! than a threshold measured against full scale, and a recording thirty decibels
//! down is below that threshold everywhere. Measured on a real pair, the quiet one
//! kept nothing at all — every frame of four and a half minutes read as silence.
//!
//! So the group is levelled before anything is decided about it. The measure is
//! EBU R128 integrated loudness rather than a plain average, because R128 gates
//! the quiet parts out: what comes back is how loud the *speech* was, which is the
//! thing two recordings of one room should agree about. An average would instead
//! report the recording with longer pauses as the quieter one and level the pauses
//! up to meet the talking.

use std::process::{Command, Stdio};

use serde::Serialize;

use crate::jobs::{Job, WorkError, WorkResult};
use crate::proc::Ffmpeg;

/// Where a levelled group ends up. Speech sits comfortably above the edit's
/// threshold here — four percent of full scale is -28 dBFS, and at -16 LUFS the
/// syllables of ordinary speech land well above it — while leaving room below
/// clipping for the peaks.
pub const DEFAULT_TARGET_LUFS: f64 = -16.0;

/// Never push a peak above this, however quiet the recording was. Gain applied to
/// a track that already peaks near full scale would only square off the loudest
/// syllables, and a clipped waveform is worse than an uneven one.
const PEAK_CEILING_DBFS: f64 = -1.0;

/// The most a recording is allowed to move. A laptop capturing its own screen
/// really does land thirty-five decibels under a lapel microphone, so the limit
/// has to clear that; past it a track is not quiet but broken — a dead microphone,
/// or the wrong stream — and lifting it further only raises room tone into the
/// edit as though it were speech. What keeps that from happening at any level is
/// the measure itself: R128 gates the quiet parts out, so a track with no speech
/// in it reports no loudness and is left exactly where it is.
const MAX_GAIN_DB: f64 = 40.0;

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Loudness {
    /// EBU R128 integrated loudness, in LUFS.
    pub integrated_lufs: f64,
    /// The loudest sample, in dBFS.
    pub peak_dbfs: f64,
}

/// How much to move this track to reach `target`, in decibels.
///
/// Held back from clipping and from absurdity, so the number is always one it is
/// safe to apply.
pub fn gain_to(target_lufs: f64, loudness: &Loudness) -> f64 {
    if !loudness.integrated_lufs.is_finite() {
        // Silence, or close enough that R128 refused to measure it. Nothing to
        // raise: there is no speech in there to bring up.
        return 0.0;
    }
    let wanted = target_lufs - loudness.integrated_lufs;
    let headroom = if loudness.peak_dbfs.is_finite() {
        PEAK_CEILING_DBFS - loudness.peak_dbfs
    } else {
        f64::INFINITY
    };
    wanted.min(headroom).clamp(-MAX_GAIN_DB, MAX_GAIN_DB)
}

/// A gain in decibels as the factor a sample is multiplied by.
pub fn as_factor(gain_db: f64) -> f64 {
    10f64.powf(gain_db / 20.0)
}

/// Per-frame levels as they would read had the track been recorded at the levelled
/// volume.
///
/// Applied to the levels rather than by decoding again: the loudness of a frame
/// scales with the gain exactly, so this is the same answer for none of the work —
/// and it leaves the cache of measured levels valid, since what is cached is still
/// what the file contains.
pub fn scale_levels(levels: &mut [u16], gain_db: f64) {
    if gain_db.abs() < 0.01 {
        return;
    }
    let factor = as_factor(gain_db);
    for level in levels.iter_mut() {
        *level = ((*level as f64) * factor).round().min(u16::MAX as f64) as u16;
    }
}

/// Measure one track the way a loudness meter would.
pub fn measure(ffmpeg: &str, path: &std::path::Path, ordinal: usize, job: &Job) -> WorkResult<Loudness> {
    let mut command = Command::new(ffmpeg);
    command
        .args(["-v", "info", "-nostdin", "-i"])
        .arg(path)
        .args([
            "-map",
            &format!("0:a:{ordinal}"),
            "-vn",
            "-sn",
            "-dn",
            "-af",
            "ebur128=peak=true",
            "-f",
            "null",
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null());

    // The meter writes its summary to stderr, which `Ffmpeg` collects for us.
    let process = Ffmpeg::spawn(&mut command, job).map_err(WorkError::Failed)?;
    let log = process.wait_for_stderr(job)?;
    parse_summary(&log).ok_or_else(|| {
        WorkError::Failed(format!(
            "could not read a loudness measurement from ffmpeg: {}",
            log.lines().rev().take(3).collect::<Vec<_>>().join(" / ")
        ))
    })
}

/// Pull the integrated loudness and true peak out of ebur128's closing summary.
///
/// The summary is indented under `Integrated loudness:` and `True peak:`, and both
/// sections carry an `I:` or a `Peak:` line. Anything else in the log — including
/// the running per-frame lines, which carry the same labels — is ignored by only
/// reading after the summary begins.
fn parse_summary(log: &str) -> Option<Loudness> {
    let summary = log.rfind("Integrated loudness:")?;
    let tail = &log[summary..];

    let mut integrated = None;
    let mut peak = None;
    for line in tail.lines() {
        let line = line.trim();
        if integrated.is_none() {
            if let Some(rest) = line.strip_prefix("I:") {
                integrated = number_before(rest, "LUFS");
            }
        }
        if peak.is_none() {
            if let Some(rest) = line.strip_prefix("Peak:") {
                peak = number_before(rest, "dBFS");
            }
        }
    }

    Some(Loudness {
        // A track with no measurable speech reports -inf, which is the truth and
        // is handled where the gain is worked out.
        integrated_lufs: integrated?,
        peak_dbfs: peak.unwrap_or(f64::NEG_INFINITY),
    })
}

fn number_before(text: &str, unit: &str) -> Option<f64> {
    let value = text.split(unit).next()?.trim();
    match value {
        "-inf" => Some(f64::NEG_INFINITY),
        other => other.parse().ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUMMARY: &str = "\
[Parsed_ebur128_0 @ 0x5] t: 4.9 M: -22.1 S: -22.6 I: -23.0 LUFS LRA: 3.4 LU
[Parsed_ebur128_0 @ 0x5] Summary:

  Integrated loudness:
    I:         -23.0 LUFS
    Threshold: -33.2 LUFS

  Loudness range:
    LRA:         3.4 LU
    Threshold: -43.1 LUFS
    LRA low:   -24.8 LUFS
    LRA high:  -21.4 LUFS

  True peak:
    Peak:       -6.2 dBFS
";

    #[test]
    fn the_summary_is_read_and_the_running_lines_are_not() {
        // The per-frame lines carry an `I:` too, and they say something different
        // at every moment; only the closing summary is the measurement.
        let measured = parse_summary(SUMMARY).expect("a summary");
        assert!((measured.integrated_lufs + 23.0).abs() < 1e-9);
        assert!((measured.peak_dbfs + 6.2).abs() < 1e-9);
    }

    #[test]
    fn a_silent_track_measures_as_no_loudness_at_all() {
        let log = "  Integrated loudness:\n    I:          -inf LUFS\n\n  True peak:\n    Peak:       -inf dBFS\n";
        let measured = parse_summary(log).expect("a summary");
        assert!(measured.integrated_lufs.is_infinite());
        // And it is left where it is, rather than lifted by the maximum.
        assert_eq!(gain_to(-16.0, &measured), 0.0);
    }

    #[test]
    fn a_log_without_a_summary_is_not_a_measurement() {
        assert!(parse_summary("Stream #0:0: Audio: aac\n").is_none());
    }

    #[test]
    fn a_quiet_recording_is_brought_up_to_the_target() {
        // The laptop case, and the reason the limit is where it is: thirty-five
        // decibels down, with room to spare below clipping, and it has to arrive
        // at the target rather than short of it.
        let quiet = Loudness { integrated_lufs: -51.0, peak_dbfs: -39.0 };
        assert!((gain_to(-16.0, &quiet) - 35.0).abs() < 1e-9);
    }

    #[test]
    fn a_loud_recording_is_brought_down() {
        let loud = Loudness { integrated_lufs: -9.0, peak_dbfs: -0.5 };
        assert!((gain_to(-16.0, &loud) + 7.0).abs() < 1e-9);
    }

    #[test]
    fn nothing_is_pushed_into_clipping() {
        // Wants +14dB to reach the target, but only has 1dB of headroom left.
        let hot = Loudness { integrated_lufs: -30.0, peak_dbfs: -2.0 };
        assert!((gain_to(-16.0, &hot) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn a_dead_microphone_is_not_amplified_into_a_conversation() {
        // Sixty decibels down is not a quiet recording, it is a broken one, and
        // lifting it that far would raise the room tone into the edit as speech.
        let dead = Loudness { integrated_lufs: -86.0, peak_dbfs: -70.0 };
        assert_eq!(gain_to(-16.0, &dead), MAX_GAIN_DB);
    }

    #[test]
    fn levels_scale_the_way_the_samples_would() {
        // 6dB is a doubling, and nothing may wrap past full scale on the way.
        let mut levels = vec![100u16, 1000, 30_000, 60_000];
        scale_levels(&mut levels, 6.0206);
        assert_eq!(levels[0], 200);
        assert_eq!(levels[1], 2000);
        assert_eq!(levels[2], 60_000);
        assert_eq!(levels[3], u16::MAX, "clamped rather than wrapped");
    }

    #[test]
    fn the_quiet_track_rises_above_the_edits_threshold() {
        // The case this exists for. Four percent of full scale is the default
        // threshold; a recording thirty decibels down sits under it everywhere,
        // and the edit therefore keeps nothing.
        let threshold = (0.04 * u16::MAX as f64).round() as u16;
        let mut speech = vec![900u16; 8]; // loud syllables, 30dB down
        assert!(speech.iter().all(|&level| level < threshold), "silent to the edit");

        let quiet = Loudness { integrated_lufs: -46.0, peak_dbfs: -34.0 };
        scale_levels(&mut speech, gain_to(DEFAULT_TARGET_LUFS, &quiet));
        assert!(speech.iter().all(|&level| level >= threshold), "heard at last");
    }

    #[test]
    fn levelling_leaves_a_track_that_is_already_right_alone() {
        let mut levels = vec![1234u16, 20_000];
        let before = levels.clone();
        scale_levels(&mut levels, 0.0);
        assert_eq!(levels, before);
    }
}
