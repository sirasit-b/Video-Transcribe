//! Lining up two recordings of the same moment by their sound.
//!
//! A camera on the person and a screen capture of the same session start whenever
//! each was pressed, so before either can be cut they have to be put on one clock.
//! Both microphones heard the same room, which is the only thing they reliably
//! share — the picture has nothing in common, and neither do the levels, the EQ or
//! the noise floor.
//!
//! So the match is made on the *shape* of the sound rather than the sound itself:
//!
//! 1. Each track is decoded to mono at a low rate and reduced to a loudness
//!    envelope — one value per few milliseconds.
//! 2. The envelope is log-compressed and standardised, which throws away exactly
//!    what the two microphones disagree about (gain, tone) and keeps what they
//!    agree on (when things got loud).
//! 3. The two envelopes are cross-correlated over every possible lag with an FFT,
//!    normalised by how much actually overlaps at each lag so a one-second sliver
//!    cannot outscore a real match.
//! 4. The peak is interpolated for sub-hop accuracy, then refined on a finer
//!    envelope near that lag.
//!
//! The answer carries how good the peak was and how far clear of the runner-up it
//! stood, because a confident wrong answer is worse than an admitted failure: two
//! recordings of *different* moments must not be silently stacked.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use rustfft::{num_complex::Complex32, FftPlanner};
use serde::Serialize;

use crate::jobs::{Job, WorkError, WorkResult};
use crate::proc::Ffmpeg;

/// Sample rate the tracks are decoded at. The envelope only needs to know when
/// sound arrived, so this is about as low as it can go while keeping speech
/// onsets crisp.
pub const DECODE_RATE: u32 = 8000;

pub struct Envelope {
    pub values: Vec<f32>,
    pub hz: f64,
    /// Length of the source in seconds, as decoded.
    pub duration: f64,
}

/// Where sound *arrived*, per hop: rising loudness, log-compressed and
/// standardised.
///
/// RMS rather than peak: a single click should not stand in for a syllable. The
/// log is what makes a quiet mic comparable to a loud one, and the standardising
/// is what lets the correlation be read as a correlation.
///
/// The rise rather than the level, because the level is the wrong signal to match
/// on. Loudness moves slowly — in a meeting, one person holds the floor for a
/// minute and the room is quiet for the next — so two loudness curves correlate
/// broadly, over tens of seconds, and the peak is a hill rather than a spike. Two
/// and a half minutes of it turned out to carry only a handful of independent
/// observations, which is not enough to call a match at all. Differencing throws
/// the slow part away and keeps the onsets, which is what the two microphones
/// really share: they heard the same syllable begin at the same instant.
fn to_envelope(samples: &[i16], rate: u32, hz: f64) -> Vec<f32> {
    let hop = ((rate as f64 / hz).round() as usize).max(1);
    if samples.is_empty() {
        return Vec::new();
    }

    let mut values: Vec<f32> = Vec::with_capacity(samples.len() / hop + 1);
    for chunk in samples.chunks(hop) {
        let energy: f64 = chunk.iter().map(|&s| (s as f64 / 32768.0).powi(2)).sum();
        let rms = (energy / chunk.len() as f64).sqrt();
        // A floor keeps silence finite; -60dB is well below anything meaningful.
        values.push((rms.max(1e-6).log10() * 20.0) as f32);
    }

    // Half-wave rectified, so a sound starting counts and the same sound stopping
    // does not: onsets are sharp and agree between microphones, while decays are
    // shaped by whatever each microphone is sitting in.
    let mut previous = values[0];
    for value in &mut values {
        let rise = (*value - previous).max(0.0);
        previous = *value;
        *value = rise;
    }

    let mean = values.iter().map(|&v| v as f64).sum::<f64>() / values.len() as f64;
    let variance = values
        .iter()
        .map(|&v| (v as f64 - mean).powi(2))
        .sum::<f64>()
        / values.len() as f64;
    let deviation = variance.sqrt();
    if deviation > 1e-9 {
        for value in &mut values {
            *value = ((*value as f64 - mean) / deviation) as f32;
        }
    } else {
        // A perfectly flat track carries no timing information at all.
        values.fill(0.0);
    }
    values
}

/// Decode a stretch of one track's audio and reduce it to an envelope.
///
/// `start` and `length` are seconds; `None` reads to the end. Seeking happens
/// before the input so a window an hour in costs nothing to reach, and
/// `-accurate_seek` is what makes it land on the sample asked for rather than the
/// nearest packet — which would put a whole keyframe interval of error straight
/// into the answer.
pub fn envelope_window(
    ffmpeg: &str,
    track: &Track,
    hz: f64,
    start: f64,
    length: Option<f64>,
    job: &Job,
) -> WorkResult<Envelope> {
    let mut command = Command::new(ffmpeg);
    command.args(["-v", "error", "-nostdin", "-accurate_seek"]);
    if start > 0.0 {
        command.args(["-ss", &format!("{start:.6}")]);
    }
    command.arg("-i").arg(&track.path);
    if let Some(length) = length {
        command.args(["-t", &format!("{length:.6}")]);
    }
    command
        .args([
            "-map",
            &format!("0:a:{}", track.ordinal),
            "-vn",
            "-sn",
            "-dn",
            "-ac",
            "1",
            "-ar",
            &DECODE_RATE.to_string(),
            "-f",
            "s16le",
            "-acodec",
            "pcm_s16le",
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped());

    let mut process = Ffmpeg::spawn(&mut command, job).map_err(WorkError::Failed)?;
    let mut stdout = process.stdout.take().expect("stdout is piped");

    let mut samples: Vec<i16> = Vec::new();
    let mut buffer = vec![0u8; 256 * 1024];
    let mut odd: Option<u8> = None;
    let outcome = (|| -> WorkResult<()> {
        loop {
            if job.is_canceled() {
                return Err(WorkError::Canceled);
            }
            let read = stdout
                .read(&mut buffer)
                .map_err(|err| WorkError::Failed(format!("failed to read audio: {err}")))?;
            if read == 0 {
                return Ok(());
            }
            let mut bytes = &buffer[..read];
            if let Some(low) = odd.take() {
                samples.push(i16::from_le_bytes([low, bytes[0]]));
                bytes = &bytes[1..];
            }
            let pairs = bytes.len() - bytes.len() % 2;
            samples.extend(
                bytes[..pairs]
                    .chunks_exact(2)
                    .map(|pair| i16::from_le_bytes([pair[0], pair[1]])),
            );
            if pairs < bytes.len() {
                odd = Some(bytes[pairs]);
            }
            job.progress.add_analyzed_frames((read / 512) as u64);
        }
    })();

    match outcome {
        Ok(()) => process.wait(job)?,
        Err(err) => {
            process.kill();
            process.discard(job);
            return Err(err);
        }
    }

    Ok(Envelope {
        duration: samples.len() as f64 / DECODE_RATE as f64,
        values: to_envelope(&samples, DECODE_RATE, hz),
        hz,
    })
}

/// Correlation of `a` against `b` at every lag, as a convolution with `b`
/// reversed. Index `m` is lag `m - (b.len() - 1)`: how far `b` sits *after* `a`.
fn correlate(a: &[f32], b: &[f32]) -> Vec<f32> {
    if a.is_empty() || b.is_empty() {
        return Vec::new();
    }
    let full = a.len() + b.len() - 1;
    let size = full.next_power_of_two();

    let mut left: Vec<Complex32> = a
        .iter()
        .map(|&v| Complex32::new(v, 0.0))
        .chain(std::iter::repeat(Complex32::new(0.0, 0.0)))
        .take(size)
        .collect();
    let mut right: Vec<Complex32> = b
        .iter()
        .rev()
        .map(|&v| Complex32::new(v, 0.0))
        .chain(std::iter::repeat(Complex32::new(0.0, 0.0)))
        .take(size)
        .collect();

    let mut planner = FftPlanner::<f32>::new();
    let forward = planner.plan_fft_forward(size);
    let inverse = planner.plan_fft_inverse(size);
    forward.process(&mut left);
    forward.process(&mut right);
    for (l, r) in left.iter_mut().zip(right.iter()) {
        *l *= *r;
    }
    inverse.process(&mut left);

    let scale = 1.0 / size as f32;
    left.into_iter().take(full).map(|c| c.re * scale).collect()
}

/// The window of `a` that has a partner in `b` when `b` sits `lag` samples
/// *later* on `a`'s clock, as a half-open range of indexes into `a`.
///
/// `lag` is the position of `b`'s first sample on `a`'s timeline, so a negative
/// lag means `b` was already rolling when `a` started.
fn overlap_window(lag: isize, la: usize, lb: usize) -> (usize, usize) {
    // b's index is a's index minus the lag, so a[k] has a partner while both
    // k and k - lag are inside their own track.
    let start = lag.clamp(0, la as isize) as usize;
    let end = (lb as isize + lag).clamp(0, la as isize) as usize;
    (start, end.max(start))
}

fn overlap_at(lag: isize, la: usize, lb: usize) -> usize {
    let (start, end) = overlap_window(lag, la, lb);
    end - start
}

/// Running sums of squares, so the energy of any window is one subtraction away.
fn prefix_squares(values: &[f32]) -> Vec<f64> {
    let mut sums = Vec::with_capacity(values.len() + 1);
    sums.push(0.0);
    let mut total = 0.0;
    for &value in values {
        total += (value as f64) * (value as f64);
        sums.push(total);
    }
    sums
}

/// How many samples an envelope has to advance before it stops resembling itself.
///
/// A loudness envelope is smooth: at 50Hz, neighbouring samples are nearly equal,
/// so a thousand of them are nowhere near a thousand independent observations.
/// This is what says how many they are worth, and it is measured from the material
/// rather than assumed, because a fast conversation and a slow monologue differ by
/// an order of magnitude.
fn correlation_span(values: &[f32]) -> usize {
    if values.len() < 4 {
        return 1;
    }
    let raw = correlate(values, values);
    let zero = values.len() - 1;
    let energy = raw[zero] as f64;
    if energy <= 1e-12 {
        return 1;
    }
    // Where it has fallen to half, capped: a tenth of the file resembling itself
    // is a pathological case, not a measurement to extrapolate from.
    let cap = (values.len() / 10).max(1);
    for span in 1..=cap {
        if (raw[zero + span] as f64 / energy) < 0.5 {
            return span;
        }
    }
    cap
}

/// Every lag scored as a normalised correlation: the overlap's dot product over
/// the geometric mean of its energies.
///
/// Dividing by the overlap length instead — which is the obvious thing, and what
/// this did first — is not bounded, and a 1.5 second sliver of two *unrelated*
/// recordings scored 1.12 that way and was reported as a confident match. Cauchy-
/// Schwarz keeps this in [-1, 1] however short the window is.
///
/// That bound alone is not enough to compare one lag with another, though, because
/// a short overlap reaches a high correlation by luck far more easily than a long
/// one. `significance` is what makes the curve comparable with itself.
struct Scores {
    /// Indexed by `lag - min_lag`.
    values: Vec<f64>,
    min_lag: isize,
    la: usize,
    lb: usize,
    /// Independent observations per overlapping sample.
    independence: f64,
}

impl Scores {
    fn of(a: &[f32], b: &[f32], min_overlap: usize) -> Option<Scores> {
        if a.is_empty() || b.is_empty() {
            return None;
        }
        let raw = correlate(a, b);
        let pa = prefix_squares(a);
        let pb = prefix_squares(b);

        let min_lag = -(b.len() as isize - 1);
        let mut values = Vec::with_capacity(raw.len());
        for index in 0..raw.len() {
            // conv(a, rev(b))[m] sums a[k] * b[k + (lb - 1 - m)], so b's index is
            // a's index plus that shift — which makes the lag its negative:
            // m - (lb - 1). A positive lag means b's start sits later on a's clock.
            let lag = index as isize - (b.len() as isize - 1);
            let (start, end) = overlap_window(lag, a.len(), b.len());
            let overlap = end - start;
            let score = if overlap < min_overlap {
                f64::NEG_INFINITY
            } else {
                let energy_a = pa[end] - pa[start];
                let b_start = (start as isize - lag) as usize;
                let energy_b = pb[b_start + overlap] - pb[b_start];
                let norm = (energy_a * energy_b).sqrt();
                if norm <= 1e-12 {
                    f64::NEG_INFINITY
                } else {
                    raw[index] as f64 / norm
                }
            };
            // Store by lag, ascending, so neighbours in the vector are neighbours
            // in time.
            values.push((lag, score));
        }
        values.sort_by_key(|(lag, _)| *lag);
        // Both sides have to be slow for the pair to be, so take the longer span:
        // the coarser of the two is what limits how much the overlap really says.
        let span = correlation_span(a).max(correlation_span(b));
        Some(Scores {
            values: values.into_iter().map(|(_, score)| score).collect(),
            min_lag,
            la: a.len(),
            lb: b.len(),
            // A run of `span` samples that resemble each other is worth about one
            // observation, either side of centre.
            independence: 1.0 / (2 * span + 1) as f64,
        })
    }

    /// How many standard deviations this lag would be, had the two recordings
    /// nothing to do with each other.
    ///
    /// Under chance, a correlation over `n` independent samples is spread about
    /// zero by roughly `1/sqrt(n)`, so multiplying by `sqrt(n)` puts every lag on
    /// one scale no matter how much of the two overlapped there.
    fn significance(&self, lag: isize) -> f64 {
        let score = self.at(lag);
        if !score.is_finite() {
            return f64::NEG_INFINITY;
        }
        let samples = overlap_at(lag, self.la, self.lb) as f64 * self.independence;
        score * samples.max(0.0).sqrt()
    }

    fn at(&self, lag: isize) -> f64 {
        let index = lag - self.min_lag;
        if index < 0 || index as usize >= self.values.len() {
            return f64::NEG_INFINITY;
        }
        self.values[index as usize]
    }

    /// The most significant lag, and how it compares with everywhere else.
    ///
    /// `exclude` is how many samples either side of the peak count as the peak
    /// itself rather than as a rival. The winner is chosen by significance, not by
    /// correlation: otherwise a sliver of overlap that happens to fit wins over
    /// the alignment that fits two minutes.
    fn peak(&self, exclude: usize) -> Option<Peak> {
        let mut best: Option<(usize, f64)> = None;
        for index in 0..self.values.len() {
            let lag = index as isize + self.min_lag;
            let significance = self.significance(lag);
            if significance.is_finite() && best.map(|(_, s)| significance > s).unwrap_or(true) {
                best = Some((index, significance));
            }
        }
        let (index, clearance) = best?;
        let lag = index as isize + self.min_lag;

        // The best alignment that is not this one. Kept for the caller to read;
        // with continuous speech it sits just above 1 even when the answer is
        // right, because a voice correlates with itself over seconds.
        let runner_up = (0..self.values.len())
            .filter(|other| (*other as isize - index as isize).unsigned_abs() > exclude)
            .map(|other| self.values[other])
            .filter(|score| score.is_finite())
            .fold(f64::NEG_INFINITY, f64::max);

        Some(Peak {
            lag,
            score: self.at(lag),
            runner_up,
            clearance,
        })
    }

    /// Sub-sample peak position, by fitting a parabola through the peak and its
    /// neighbours — the standard way to read a correlation peak finer than the
    /// grid it was computed on.
    fn sub_sample(&self, lag: isize) -> f64 {
        let (left, middle, right) = (self.at(lag - 1), self.at(lag), self.at(lag + 1));
        if !left.is_finite() || !right.is_finite() {
            return 0.0;
        }
        let denominator = 2.0 * middle - left - right;
        if denominator.abs() < 1e-12 {
            return 0.0;
        }
        (0.5 * (left - right) / denominator).clamp(-1.0, 1.0)
    }
}

struct Peak {
    lag: isize,
    score: f64,
    runner_up: f64,
    clearance: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct SyncResult {
    /// Where the second recording's start sits on the first one's clock, in
    /// seconds. Negative means it was already rolling when the first one started.
    /// A time `t` on the first recording is `t - offset_seconds` on the second.
    pub offset_seconds: f64,
    /// The normalised correlation at that lag, in [-1, 1].
    pub confidence: f64,
    /// The peak over the best alignment away from it. Reported because it is easy
    /// to read, but not what the decision rests on.
    pub peak_ratio: f64,
    /// How many standard deviations the peak stands above the rest of the curve.
    /// This is what separates a real match from a rolling baseline.
    pub clearance: f64,

    /// Seconds the two recordings share once aligned.
    pub overlap_seconds: f64,
    /// Whether this is good enough to stack the two without checking by ear.
    pub reliable: bool,
    pub coarse_hz: f64,
    pub fine_hz: f64,
    pub a_duration: f64,
    pub b_duration: f64,
}

/// Barely a floor. The decision rests on `MIN_CLEARANCE`, which already accounts
/// for how much material stands behind the score; this only rules out the
/// degenerate case of a track that correlates with nothing because it holds
/// almost nothing. A real pair can score low here and still be certain: at a
/// millisecond a hop, two microphones at different distances disagree about the
/// exact instant of every onset, which blunts the correlation without moving the
/// peak — and refusing those was what stopped the refinement from ever running.
const MIN_CONFIDENCE: f64 = 0.05;
/// Six standard deviations under the chance model. High enough that nothing
/// accidental reaches it, low enough for a noisy room through a bad microphone.
const MIN_CLEARANCE: f64 = 6.0;
/// Lags within this many seconds of the peak are part of it, not rivals to it.
const PEAK_WIDTH_SECONDS: f64 = 2.0;

/// Find how far apart two recordings started.
///
/// `a` and `b` are envelopes at the same rate, found over the whole file. `fine`
/// is an optional pair at a higher rate, searched only around the coarse answer —
/// which is what turns "about right" into a few milliseconds.
pub fn find_offset(
    a: &Envelope,
    b: &Envelope,
    fine: Option<(&Envelope, &Envelope)>,
    min_overlap_seconds: f64,
) -> Option<SyncResult> {
    let min_overlap = ((min_overlap_seconds * a.hz).round() as usize).max(1);
    let scores = Scores::of(&a.values, &b.values, min_overlap)?;
    let exclude = (a.hz * PEAK_WIDTH_SECONDS).round() as usize;
    let peak = scores.peak(exclude)?;
    let (lag, confidence, runner_up) = (peak.lag, peak.score, peak.runner_up);

    let mut offset = (lag as f64 + scores.sub_sample(lag)) / a.hz;
    let mut fine_hz = a.hz;

    if let Some((fine_a, fine_b)) = fine {
        let fine_min = ((min_overlap_seconds * fine_a.hz).round() as usize).max(1);
        if let Some(fine_scores) = Scores::of(&fine_a.values, &fine_b.values, fine_min) {
            // Search only within a coarse hop of the answer we already have.
            let window = (fine_a.hz / a.hz).ceil() as isize + 1;
            let centre = (offset * fine_a.hz).round() as isize;
            let mut best: Option<(isize, f64)> = None;
            for candidate in (centre - window)..=(centre + window) {
                let score = fine_scores.at(candidate);
                if score.is_finite() && best.map(|(_, b)| score > b).unwrap_or(true) {
                    best = Some((candidate, score));
                }
            }
            if let Some((candidate, _)) = best {
                offset = (candidate as f64 + fine_scores.sub_sample(candidate)) / fine_a.hz;
                fine_hz = fine_a.hz;
            }
        }
    }

    let overlap = overlap_at((offset * a.hz).round() as isize, a.values.len(), b.values.len());
    let peak_ratio = if runner_up.is_finite() && runner_up > 1e-6 {
        confidence / runner_up
    } else if confidence > 0.0 {
        f64::INFINITY
    } else {
        0.0
    };

    Some(SyncResult {
        offset_seconds: offset,
        confidence,
        peak_ratio,
        clearance: peak.clearance,
        overlap_seconds: overlap as f64 / a.hz,
        reliable: confidence >= MIN_CONFIDENCE && peak.clearance >= MIN_CLEARANCE,
        coarse_hz: a.hz,
        fine_hz,
        a_duration: a.duration,
        b_duration: b.duration,
    })
}

// ---------------------------------------------------------------------------
// Measuring a pair
// ---------------------------------------------------------------------------

/// Envelope rate for the search across the whole file. 20ms a hop keeps three
/// hours down to half a million samples, and speech onsets still land in the
/// right hop.
pub const COARSE_HZ: f64 = 50.0;
/// Envelope rate for the refinement, which only ever runs on a short excerpt.
/// 1kHz puts the answer inside a millisecond — well under the video frame the
/// cut is quantised to in the end.
pub const FINE_HZ: f64 = 1000.0;

/// Seconds of material one refinement window looks at.
const WINDOW_SECONDS: f64 = 30.0;
/// How far a refinement may move the coarse answer: one coarse hop, plus room
/// for the two clocks to have pulled apart since the start.
const REFINE_SLACK: f64 = 2.0;
/// Below this there is not enough shared material to fit a line through, so drift
/// is left unmeasured rather than guessed from two nearby windows.
const MIN_DRIFT_OVERLAP: f64 = 240.0;

/// Which audio of which file.
#[derive(Clone, Debug)]
pub struct Track {
    pub path: PathBuf,
    pub ordinal: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct Refinement {
    /// Centre of the window this was measured in, on the first recording's clock.
    pub at_seconds: f64,
    pub offset_seconds: f64,
    pub confidence: f64,
    pub clearance: f64,
}

/// Whether the two recorders' clocks run at the same speed.
///
/// A camera and a screen recorder each time themselves, and a hundredth of a
/// percent apart is ordinary — which is a second of slip an hour. One offset
/// cannot see that; several across the overlap can, as a slope.
#[derive(Clone, Debug, Serialize)]
pub struct Drift {
    /// Parts per million the second recording runs slow, which is the same thing
    /// as how fast the offset between them grows. Positive means it falls
    /// progressively behind the first.
    pub ppm: f64,
    pub seconds_per_hour: f64,
    /// The fitted line, kept so the pair can be cut through it rather than
    /// through a single offset that is only right in the middle.
    pub slope: f64,
    pub intercept_seconds: f64,
    /// Largest distance between a measured window and the fitted line. A big
    /// residual means the slope is not a clean drift and should not be trusted.
    pub residual_seconds: f64,
    /// Whether the slope is worth correcting: measured cleanly, and enough to
    /// see. A frame at 30fps is 33ms, so a slip under about 10ms across the whole
    /// overlap is below the resolution the cut has anyway.
    pub significant: bool,
    pub windows: Vec<Refinement>,
}

/// JSON has no infinity: serde writes one as `null`, which reaches a browser as a
/// missing number rather than a large one. A quantity with no bound is reported as
/// absent on purpose instead of as a value that turns into nothing in transit.
fn finite(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}

#[derive(Clone, Debug, Serialize)]
pub struct SyncReport {
    /// How many standard deviations the correlation peak stood above the rest.
    /// Absent when nothing was measured, which is the case for an offset that was
    /// handed to us.
    pub clearance: Option<f64>,
    /// Where the second recording's start sits on the first one's clock. Negative
    /// means it was already rolling. A time `t` on the first is `t - offset` on
    /// the second.
    pub offset_seconds: f64,
    pub confidence: f64,
    pub peak_ratio: Option<f64>,
    pub overlap_seconds: f64,
    pub reliable: bool,
    /// Whether this was measured or handed to us. A supplied offset is taken as
    /// read — someone who typed it in has decided they trust it — but it carries
    /// none of the evidence a measurement does, and must not be mistaken for one.
    pub supplied: bool,
    /// The whole-file answer, before any refinement, for comparison.
    pub coarse_offset_seconds: f64,
    pub coarse_hz: f64,
    pub fine_hz: f64,
    pub a_duration: f64,
    pub b_duration: f64,
    pub drift: Option<Drift>,
    /// The excerpt the answer was sharpened on, when one worked.
    pub refined: Option<Refinement>,
    /// Why the answer should not be trusted, when it should not be.
    pub warning: Option<String>,
}

impl SyncReport {
    /// A report for an offset that was handed to us rather than measured.
    pub fn supplied(offset_seconds: f64, a_duration: f64, b_duration: f64) -> SyncReport {
        let mut report = SyncReport {
            offset_seconds,
            confidence: 1.0,
            peak_ratio: None,
            clearance: None,
            supplied: true,
            overlap_seconds: 0.0,
            reliable: true,
            coarse_offset_seconds: offset_seconds,
            coarse_hz: 0.0,
            fine_hz: 0.0,
            a_duration,
            b_duration,
            drift: None,
            refined: None,
            warning: None,
        };
        let (start, end) = report.overlap_on_a();
        report.overlap_seconds = end - start;
        report
    }

    /// The stretch both recordings cover, on the first one's clock.
    pub fn overlap_on_a(&self) -> (f64, f64) {
        let start = self.offset_seconds.max(0.0);
        let end = (self.b_duration + self.offset_seconds).min(self.a_duration);
        (start, end.max(start))
    }
}

/// Refine an offset we already roughly believe, by correlating one excerpt of
/// each recording at a much finer rate.
///
/// Decoding a whole three-hour file at 1kHz would be ten million samples and a
/// gigabyte of FFT; thirty seconds of it is thirty thousand. The excerpt is only
/// meaningful because `base_offset` already says which thirty seconds of the
/// second recording to line it up against.
fn refine_at(
    ffmpeg: &str,
    a: &Track,
    b: &Track,
    job: &Job,
    base_offset: f64,
    window_start: f64,
    window_length: f64,
    a_duration: f64,
    b_duration: f64,
) -> WorkResult<Option<Refinement>> {
    let window_start = window_start.max(0.0);
    let length = window_length.min(a_duration - window_start);
    if length < 2.0 {
        return Ok(None);
    }
    // The excerpt of b that should hold the same moments, plus slack either side
    // for the coarse answer to be wrong within.
    let b_start = (window_start - base_offset - REFINE_SLACK).max(0.0);
    let b_length = (length + 2.0 * REFINE_SLACK).min(b_duration - b_start);
    if b_length < length.min(4.0) {
        return Ok(None);
    }

    let excerpt_a = envelope_window(ffmpeg, a, FINE_HZ, window_start, Some(length), job)?;
    let excerpt_b = envelope_window(ffmpeg, b, FINE_HZ, b_start, Some(b_length), job)?;
    // Most of the shorter excerpt has to take part, so a window that happens to
    // be silent on one side cannot answer from a sliver.
    let min_overlap = (length.min(b_length) * 0.5).min(10.0);
    let found = match find_offset(&excerpt_a, &excerpt_b, None, min_overlap) {
        Some(found) if found.reliable => found,
        _ => return Ok(None),
    };

    // `found.offset_seconds` is b's excerpt start on a's excerpt clock, and both
    // excerpts know where they were cut from, so undo the two starts.
    Ok(Some(Refinement {
        at_seconds: window_start + length / 2.0,
        offset_seconds: found.offset_seconds + window_start - b_start,
        confidence: found.confidence,
        clearance: found.clearance,
    }))
}

/// Least-squares slope and intercept of `points`, as (slope, intercept).
fn fit_line(points: &[(f64, f64)]) -> Option<(f64, f64)> {
    if points.len() < 2 {
        return None;
    }
    let n = points.len() as f64;
    let mean_x = points.iter().map(|(x, _)| x).sum::<f64>() / n;
    let mean_y = points.iter().map(|(_, y)| y).sum::<f64>() / n;
    let mut covariance = 0.0;
    let mut variance = 0.0;
    for (x, y) in points {
        covariance += (x - mean_x) * (y - mean_y);
        variance += (x - mean_x).powi(2);
    }
    if variance < 1e-9 {
        return None;
    }
    let slope = covariance / variance;
    Some((slope, mean_y - slope * mean_x))
}

/// Line up two recordings of the same moment.
///
/// Runs the whole-file search first, then refines it on an excerpt, then — when
/// there is enough shared material — measures the offset at several points to see
/// whether the two clocks keep the same time.
pub fn measure(
    ffmpeg: &str,
    a: &Track,
    b: &Track,
    a_duration: f64,
    b_duration: f64,
    check_drift: bool,
    job: &Job,
) -> WorkResult<SyncReport> {
    let coarse_a = envelope_window(ffmpeg, a, COARSE_HZ, 0.0, None, job)?;
    let coarse_b = envelope_window(ffmpeg, b, COARSE_HZ, 0.0, None, job)?;

    // Two recordings of one session should share minutes, but a short pair is
    // legitimate; ask for a tenth of the shorter one, and at least a second.
    let min_overlap = (coarse_a.duration.min(coarse_b.duration) * 0.1).clamp(1.0, 30.0);
    let coarse = find_offset(&coarse_a, &coarse_b, None, min_overlap).ok_or_else(|| {
        WorkError::Failed("there is no audio in these two files to line up".to_string())
    })?;

    let mut report = SyncReport {
        offset_seconds: coarse.offset_seconds,
        confidence: coarse.confidence,
        peak_ratio: finite(coarse.peak_ratio),
        clearance: finite(coarse.clearance),
        supplied: false,
        overlap_seconds: coarse.overlap_seconds,
        reliable: coarse.reliable,
        coarse_offset_seconds: coarse.offset_seconds,
        coarse_hz: COARSE_HZ,
        fine_hz: COARSE_HZ,
        a_duration,
        b_duration,
        drift: None,
        refined: None,
        warning: None,
    };

    if !coarse.reliable {
        // Refining a wrong answer only makes it a precise wrong answer.
        report.warning = Some(if coarse.confidence < MIN_CONFIDENCE {
            "the two soundtracks do not match well enough to line up automatically; \
             they may be recordings of different moments, or one may be silent"
                .to_string()
        } else {
            "several places line up about equally well, so the match is ambiguous; \
             a repetitive or looping soundtrack does this"
                .to_string()
        });
        return Ok(report);
    }

    // Refine in the middle of the overlap, where both recordings are running.
    let (overlap_start, overlap_end) = report.overlap_on_a();
    let overlap = overlap_end - overlap_start;
    let centre = overlap_start + (overlap - WINDOW_SECONDS.min(overlap)) / 2.0;
    if let Some(refined) = refine_at(
        ffmpeg,
        a,
        b,
        job,
        coarse.offset_seconds,
        centre,
        WINDOW_SECONDS,
        a_duration,
        b_duration,
    )? {
        // A refinement that lands far from the coarse answer is not a refinement;
        // keep the whole-file search, which had all the material to work with.
        if (refined.offset_seconds - coarse.offset_seconds).abs() <= REFINE_SLACK {
            report.offset_seconds = refined.offset_seconds;
            report.fine_hz = FINE_HZ;
            report.refined = Some(refined);
        }
    }

    if check_drift && overlap >= MIN_DRIFT_OVERLAP {
        // Spread the windows across the overlap rather than bunching them: the
        // slope is only as good as the distance between the first and the last.
        let mut windows = Vec::new();
        for step in 0..5 {
            let fraction = 0.08 + 0.84 * step as f64 / 4.0;
            let start = (overlap_start + overlap * fraction - WINDOW_SECONDS / 2.0)
                .clamp(overlap_start, (overlap_end - WINDOW_SECONDS).max(overlap_start));
            if let Some(found) = refine_at(
                ffmpeg,
                a,
                b,
                job,
                report.offset_seconds,
                start,
                WINDOW_SECONDS,
                a_duration,
                b_duration,
            )? {
                windows.push(found);
            }
        }

        if windows.len() >= 3 {
            let points: Vec<(f64, f64)> = windows
                .iter()
                .map(|w| (w.at_seconds, w.offset_seconds))
                .collect();
            if let Some((slope, intercept)) = fit_line(&points) {
                let residual = points
                    .iter()
                    .map(|(x, y)| (y - (slope * x + intercept)).abs())
                    .fold(0.0, f64::max);
                // How far the slope carries the two apart over the whole overlap.
                let total = slope.abs() * overlap;
                report.drift = Some(Drift {
                    ppm: slope * 1e6,
                    seconds_per_hour: slope * 3600.0,
                    slope,
                    intercept_seconds: intercept,
                    residual_seconds: residual,
                    significant: residual < 0.02 && total > 0.01,
                    windows,
                });
                // Read the offset in the middle of the overlap, where the fitted
                // line is best determined.
                let middle = overlap_start + overlap / 2.0;
                report.offset_seconds = slope * middle + intercept;
            }
        }
    }

    Ok(report)
}

/// The map from the first recording's clock to the second one's.
///
/// One number is enough when both recorders keep the same time. When they do not,
/// the offset itself moves as the recording goes on, and carrying the slope here
/// is what keeps the far end of a three-hour pair together.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Clock {
    /// The offset at time zero on the first recording.
    pub offset_seconds: f64,
    /// How much the offset grows per second of the first recording.
    pub slope: f64,
}

impl Clock {
    /// The moment on the second recording that matches `a_time` on the first.
    pub fn to_b(&self, a_time: f64) -> f64 {
        a_time - (self.offset_seconds + self.slope * a_time)
    }

    /// The offset as it reads at `a_time`.
    pub fn offset_at(&self, a_time: f64) -> f64 {
        self.offset_seconds + self.slope * a_time
    }
}

impl SyncReport {
    /// How to convert times between the pair. Follows the measured drift only
    /// when the drift was measured cleanly and is big enough to matter; a noisy
    /// slope fitted through five windows would otherwise be extrapolated across
    /// the whole recording, which can only make things worse.
    pub fn clock(&self) -> Clock {
        match &self.drift {
            Some(drift) if drift.significant => Clock {
                offset_seconds: drift.intercept_seconds,
                slope: drift.slope,
            },
            _ => Clock {
                offset_seconds: self.offset_seconds,
                slope: 0.0,
            },
        }
    }
}

/// One recording's levels read on the other one's frames.
///
/// Each frame of the first recording asks the second one how loud it was at the
/// same moment, so a decision can be made about a moment rather than about a
/// file. Frames outside what the second recording covers read as silence, which is
/// the truth: it was not there.
pub fn map_levels(
    levels: &[u16],
    b_timebase: f64,
    clock: &Clock,
    a_frames: usize,
    a_timebase: f64,
) -> Vec<u16> {
    (0..a_frames)
        .map(|frame| {
            let b_time = clock.to_b(frame as f64 / a_timebase);
            if b_time < 0.0 {
                return 0;
            }
            let index = (b_time * b_timebase).round() as usize;
            levels.get(index).copied().unwrap_or(0)
        })
        .collect()
}

/// Cut the kept ranges back to the stretch both recordings cover.
///
/// Outside the overlap only one of the two exists, and a pair of exports where
/// one holds moments the other cannot is a pair that slips out of step from the
/// first missing frame onward. Losing the ends is the price of both files being
/// the same length and the same moments all the way through.
pub fn clamp_to_overlap(
    segments: &[(usize, usize)],
    timebase: f64,
    overlap: (f64, f64),
) -> Vec<(usize, usize)> {
    let (first, last) = shared_frames(overlap, timebase);
    segments
        .iter()
        .filter_map(|&(start, end)| {
            let start = start.max(first);
            let end = end.min(last);
            (end > start).then_some((start, end))
        })
        .collect()
}

/// The overlap as a half-open range of the first recording's frames.
///
/// Inward at both ends: a frame only counts as shared if all of it is.
pub fn shared_frames(overlap: (f64, f64), timebase: f64) -> (usize, usize) {
    let first = (overlap.0 * timebase).ceil().max(0.0) as usize;
    let last = (overlap.1 * timebase).floor().max(0.0) as usize;
    (first, last.max(first))
}

/// The same kept moments, counted in the second recording's frames.
///
/// Two things have to hold at once, and they are not the same thing:
///
/// * each range has to *begin* on the moment the first recording begins it, which
///   is what the clock converts — and converting each range where it sits is also
///   what absorbs a drifting clock, without resampling anyone's audio;
/// * the two exports have to stay the same length as each other all the way
///   through, or they slide apart in whatever timeline they are stacked on.
///
/// Rounding each range's own end to the second recording's frames satisfies the
/// first and quietly breaks the second, because the two frame rates round
/// differently and the error accumulates: fifty-eight cuts at 30 and 25fps came
/// out 107ms apart. So each range's *length* is measured against the running total
/// of the shared timeline instead of against its own end, which holds the two
/// within half a frame of each other no matter how many cuts there are.
pub fn shift_segments(
    segments: &[(usize, usize)],
    a_timebase: f64,
    b_timebase: f64,
    clock: &Clock,
    b_total_frames: usize,
) -> Vec<(usize, usize)> {
    let mut shifted = Vec::with_capacity(segments.len());
    let mut kept_a = 0usize;
    let mut kept_b = 0usize;
    for &(start, end) in segments {
        // A range that begins before the second recording did has no counterpart
        // in it. Pinning it to the start instead would keep the range and lose
        // the moment, which is the one thing a synced cut must not do.
        let b_start = (clock.to_b(start as f64 / a_timebase) * b_timebase).round();
        if b_start < 0.0 {
            continue;
        }
        let b_start = b_start as usize;
        kept_a += end - start;
        // Where the second recording's output has to stand once this range is in
        // it: the same amount of time as the first one holds by then.
        let target = (kept_a as f64 / a_timebase * b_timebase).round() as usize;
        let length = target.saturating_sub(kept_b);
        let b_end = (b_start + length).min(b_total_frames);
        if b_end > b_start {
            shifted.push((b_start, b_end));
            kept_b += b_end - b_start;
        }
    }
    shifted
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Speech-like material: bursts of loud at irregular spacing, which is what
    /// the correlation keys on. Irregular matters — a periodic take lines up
    /// equally well at every period, and no method can choose between those.
    fn bursts(hops: usize, seed: u64) -> Vec<f32> {
        let mut state = seed | 1;
        let mut next = |bound: u64| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (state >> 33) % bound
        };
        let mut out = Vec::with_capacity(hops);
        while out.len() < hops {
            let quiet = 5 + next(40) as usize;
            let loud = 5 + next(60) as usize;
            let level = 0.3 + next(700) as f32 / 1000.0;
            out.extend(std::iter::repeat(0.01).take(quiet));
            out.extend(std::iter::repeat(level).take(loud));
        }
        out.truncate(hops);
        out
    }

    fn standardise(values: &[f32]) -> Vec<f32> {
        let mean = values.iter().map(|&v| v as f64).sum::<f64>() / values.len() as f64;
        let deviation = (values
            .iter()
            .map(|&v| (v as f64 - mean).powi(2))
            .sum::<f64>()
            / values.len() as f64)
            .sqrt();
        values
            .iter()
            .map(|&v| ((v as f64 - mean) / deviation) as f32)
            .collect()
    }

    fn envelope(values: Vec<f32>, hz: f64) -> Envelope {
        Envelope {
            duration: values.len() as f64 / hz,
            values,
            hz,
        }
    }

    #[test]
    fn correlation_matches_the_definition() {
        // The FFT route has to agree with the textbook sum, term for term.
        let a = [1.0f32, 2.0, 3.0, 4.0];
        let b = [0.5f32, 1.5, -1.0];
        let fast = correlate(&a, &b);

        // `correlate` is a convolution with b reversed: index m holds the sum
        // over a[k] * b[k + (lb - 1 - m)].
        for (m, &value) in fast.iter().enumerate() {
            let shift = b.len() as isize - 1 - m as isize;
            let mut expected = 0.0f32;
            for (k, &av) in a.iter().enumerate() {
                let j = k as isize + shift;
                if j >= 0 && (j as usize) < b.len() {
                    expected += av * b[j as usize];
                }
            }
            assert!((value - expected).abs() < 1e-3, "at {m}: {value} vs {expected}");
        }
    }

    #[test]
    fn a_recording_that_started_earlier_reports_a_negative_offset() {
        // b holds the same take but was rolling 120 hops (600ms at 200Hz) before
        // a started, so a's first moment is 120 hops into b.
        let a = standardise(&bursts(4000, 7));
        let mut b = vec![0.01f32; 120];
        b.extend_from_slice(&a[..a.len() - 120]);
        let b = standardise(&b);

        let result = find_offset(&envelope(a, 200.0), &envelope(b, 200.0), None, 2.0).unwrap();
        assert!((result.offset_seconds + 0.6).abs() < 0.01, "{result:?}");
        assert!(result.reliable, "{result:?}");
        assert!(result.confidence > 0.8 && result.confidence <= 1.0, "{result:?}");
        assert!(result.clearance > MIN_CLEARANCE, "{result:?}");
    }

    #[test]
    fn a_recording_that_started_later_reports_a_positive_offset() {
        let full = standardise(&bursts(4000, 11));
        // a is the whole take; b missed the first 200 hops (1s) of it.
        let b = standardise(&full[200..]);
        let result = find_offset(&envelope(full, 200.0), &envelope(b, 200.0), None, 2.0).unwrap();
        assert!((result.offset_seconds - 1.0).abs() < 0.01, "{result:?}");
    }

    #[test]
    fn a_quieter_microphone_finds_the_same_moment() {
        // Different gain and a different noise floor: standardising the envelope
        // is what makes the two comparable at all.
        let loud = bursts(4000, 21);
        let mut quiet: Vec<f32> = loud.iter().map(|&v| v * 0.15 + 0.02).collect();
        for (index, value) in quiet.iter_mut().enumerate() {
            *value += ((index * 7919 % 100) as f32 / 100.0 - 0.5) * 0.01;
        }
        let delayed: Vec<f32> = std::iter::repeat(0.02).take(64).chain(quiet).collect();

        let result = find_offset(
            &envelope(standardise(&loud), 200.0),
            &envelope(standardise(&delayed), 200.0),
            None,
            2.0,
        )
        .unwrap();
        assert!((result.offset_seconds + 0.32).abs() < 0.01, "{result:?}");
        assert!(result.reliable, "{result:?}");
    }

    #[test]
    fn a_normalised_score_cannot_exceed_one() {
        // Cauchy-Schwarz bounds it, and the bound is what stops a short overlap
        // from buying a high score.
        let a = standardise(&bursts(3000, 31));
        let b = standardise(&bursts(1500, 77));
        let scores = Scores::of(&a, &b, 200).unwrap();
        for lag in -(b.len() as isize - 1)..(a.len() as isize) {
            let score = scores.at(lag);
            if score.is_finite() {
                assert!(score <= 1.0 + 1e-6 && score >= -1.0 - 1e-6, "{score} at lag {lag}");
            }
        }
    }

    #[test]
    fn unrelated_recordings_are_reported_as_unreliable() {
        let a = standardise(&bursts(4000, 3));
        let b = standardise(&bursts(4000, 9999));
        let result = find_offset(&envelope(a, 200.0), &envelope(b, 200.0), None, 5.0).unwrap();
        assert!(
            !result.reliable,
            "two unrelated takes must not be called a match: {result:?}"
        );
    }

    #[test]
    fn a_sliver_of_overlap_cannot_win() {
        // Identical content, but b is only 150ms long. With a 2s minimum overlap
        // nothing qualifies, so there is nothing to report.
        let a = standardise(&bursts(2000, 5));
        let b = standardise(&a[..30].to_vec());
        let result = find_offset(&envelope(a, 200.0), &envelope(b, 200.0), None, 2.0);
        assert!(result.is_none() || !result.unwrap().reliable);
    }

    #[test]
    fn a_lucky_sliver_does_not_outrank_the_real_alignment() {
        // b is two seconds of a, prefixed with material that appears nowhere in
        // a. Hanging off the front of a, the prefix can be ignored and the two
        // real seconds fitted wherever they like; lined up properly, all of b
        // takes part and only the two seconds match. The sliver can reach a
        // higher correlation, so the answer has to be decided on more than that.
        let a = standardise(&bursts(6000, 13));
        let mut b = bursts(400, 999);
        b.extend_from_slice(&a[3000..3400]);
        let b = standardise(&b);

        let scores = Scores::of(&a, &b, 100).unwrap();
        let truth = 3000 - 400;
        let peak = scores.peak(400).unwrap();
        assert_eq!(peak.lag, truth as isize, "chose lag {} over {truth}", peak.lag);

        // And the thing that made the difference: a short overlap that scores
        // higher is still worth less.
        let sliver = -(b.len() as isize) + 450;
        assert!(
            scores.significance(truth as isize) > scores.significance(sliver),
            "the real alignment has to be the more significant one"
        );
    }

    #[test]
    fn the_fine_pass_sharpens_the_answer() {
        // Coarse at 50Hz can only land within 20ms; the fine pass at 500Hz has to
        // do better on the same material.
        let fine_a = standardise(&bursts(20_000, 41));
        let mut fine_b = vec![0.01f32; 137]; // 274ms at 500Hz
        fine_b.extend_from_slice(&fine_a[..fine_a.len() - 137]);
        let fine_b = standardise(&fine_b);

        let decimate = |values: &[f32]| -> Vec<f32> {
            values.chunks(10).map(|c| c.iter().sum::<f32>() / c.len() as f32).collect()
        };
        let coarse_a = envelope(standardise(&decimate(&fine_a)), 50.0);
        let coarse_b = envelope(standardise(&decimate(&fine_b)), 50.0);
        let fine_a = envelope(fine_a, 500.0);
        let fine_b = envelope(fine_b, 500.0);

        let truth = -137.0 / 500.0; // b was rolling first
        let coarse_only = find_offset(&coarse_a, &coarse_b, None, 2.0).unwrap();
        let refined = find_offset(&coarse_a, &coarse_b, Some((&fine_a, &fine_b)), 2.0).unwrap();
        assert!(
            (refined.offset_seconds - truth).abs() <= (coarse_only.offset_seconds - truth).abs() + 1e-9,
            "the fine pass should not be worse: {refined:?} vs {coarse_only:?}"
        );
        assert!((refined.offset_seconds - truth).abs() < 0.005, "{refined:?}");
        assert_eq!(refined.fine_hz, 500.0);
    }

    #[test]
    fn an_envelope_marks_where_sound_arrived() {
        // 8000 samples at 200Hz is 40 samples a hop, so 200 hops.
        let samples: Vec<i16> = (0..8000)
            .map(|i| if i % 2000 < 1000 { 0 } else { 20000 })
            .collect();
        let values = to_envelope(&samples, 8000, 200.0);
        assert_eq!(values.len(), 200);
        let mean = values.iter().map(|&v| v as f64).sum::<f64>() / values.len() as f64;
        assert!(mean.abs() < 1e-5, "mean {mean}");

        // Four bursts in 8000 samples at 40 samples a hop: silence for 25 hops,
        // sound for 25, and only the hop where it arrives stands out.
        let loud: Vec<usize> = values
            .iter()
            .enumerate()
            .filter(|(_, &v)| v > 1.0)
            .map(|(index, _)| index)
            .collect();
        assert_eq!(loud, vec![25, 75, 125, 175], "only the onsets");

        // Turning the gain down must not change the shape, which is the whole
        // reason the envelope is in dB and standardised.
        let quieter: Vec<i16> = samples.iter().map(|&s| s / 8).collect();
        let quiet_values = to_envelope(&quieter, 8000, 200.0);
        for (a, b) in values.iter().zip(quiet_values.iter()) {
            assert!((a - b).abs() < 1e-3, "{a} vs {b}");
        }
    }

    #[test]
    fn a_silent_track_carries_no_timing() {
        let values = to_envelope(&vec![0i16; 8000], 8000, 200.0);
        assert!(values.iter().all(|&v| v == 0.0));
    }

    #[test]
    fn an_offset_given_by_hand_claims_no_evidence() {
        // Infinity here used to mean "nothing to beat", which JSON turns into
        // null on the way out — a missing number the far end then reads as a
        // number that is there. Absent on purpose, and marked as supplied.
        let report = SyncReport::supplied(2.5, 100.0, 90.0);
        assert!(report.clearance.is_none() && report.peak_ratio.is_none());
        assert!(report.supplied && report.reliable);
        assert!((report.overlap_seconds - 90.0).abs() < 1e-9);

        let json = serde_json::to_string(&report).expect("serialises");
        assert!(json.contains("\"supplied\":true"), "{json}");
        assert!(json.contains("\"clearance\":null"), "{json}");
        // Every number that survives the trip is a real one.
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parses");
        for (key, value) in parsed.as_object().expect("an object") {
            if let Some(number) = value.as_f64() {
                assert!(number.is_finite(), "{key} is {number}");
            }
        }
    }

    #[test]
    fn a_clock_without_drift_shifts_every_moment_the_same() {
        let clock = Clock { offset_seconds: 2.5, slope: 0.0 };
        // b started 2.5s after a, so a-time 10 is b-time 7.5.
        assert!((clock.to_b(10.0) - 7.5).abs() < 1e-9);
        assert!((clock.to_b(0.0) + 2.5).abs() < 1e-9);
    }

    #[test]
    fn a_drifting_clock_pulls_further_apart_as_it_goes() {
        // 100ppm: a second of slip every ten thousand seconds.
        let clock = Clock { offset_seconds: 0.0, slope: 100e-6 };
        assert!((clock.offset_at(0.0)).abs() < 1e-12);
        assert!((clock.offset_at(3600.0) - 0.36).abs() < 1e-9);
        assert!((clock.to_b(3600.0) - 3599.64).abs() < 1e-6);
    }

    #[test]
    fn the_clock_follows_a_clean_drift_and_ignores_a_noisy_one() {
        let base = SyncReport {
            offset_seconds: 1.0,
            confidence: 0.9,
            peak_ratio: Some(3.0),
            clearance: Some(25.0),
            supplied: false,
            overlap_seconds: 600.0,
            reliable: true,
            coarse_offset_seconds: 1.0,
            coarse_hz: COARSE_HZ,
            fine_hz: FINE_HZ,
            a_duration: 600.0,
            b_duration: 600.0,
            drift: None,
            refined: None,
            warning: None,
        };
        let drift = |significant| Drift {
            ppm: 100.0,
            seconds_per_hour: 0.36,
            slope: 100e-6,
            intercept_seconds: 0.5,
            residual_seconds: 0.001,
            significant,
            windows: Vec::new(),
        };

        let followed = SyncReport { drift: Some(drift(true)), ..base.clone() };
        assert_eq!(followed.clock().slope, 100e-6);
        assert_eq!(followed.clock().offset_seconds, 0.5);

        // Measured badly, so the one offset stands.
        let ignored = SyncReport { drift: Some(drift(false)), ..base.clone() };
        assert_eq!(ignored.clock().slope, 0.0);
        assert_eq!(ignored.clock().offset_seconds, 1.0);
        assert_eq!(base.clock().slope, 0.0);
    }

    #[test]
    fn the_overlap_is_what_both_recordings_cover() {
        let report = SyncReport {
            offset_seconds: 5.0,
            confidence: 0.9,
            peak_ratio: Some(3.0),
            clearance: Some(25.0),
            supplied: false,
            overlap_seconds: 0.0,
            reliable: true,
            coarse_offset_seconds: 5.0,
            coarse_hz: COARSE_HZ,
            fine_hz: FINE_HZ,
            a_duration: 100.0,
            b_duration: 60.0,
            drift: None,
            refined: None,
            warning: None,
        };
        // b came in 5s late and ran out after 60s of its own.
        let (start, end) = report.overlap_on_a();
        assert!((start - 5.0).abs() < 1e-9);
        assert!((end - 65.0).abs() < 1e-9);

        // Pushed apart entirely, there is nothing shared and no negative span.
        let apart = SyncReport { offset_seconds: 200.0, ..report };
        let (start, end) = apart.overlap_on_a();
        assert!(end >= start && (end - start) < 1e-9);
    }

    #[test]
    fn clamping_keeps_only_the_shared_frames() {
        // 25fps, shared from 2s to 8s: frames 50 to 200.
        let segments = [(0usize, 60usize), (100, 150), (190, 400)];
        let kept = clamp_to_overlap(&segments, 25.0, (2.0, 8.0));
        assert_eq!(kept, vec![(50, 60), (100, 150), (190, 200)]);

        // A range wholly outside is dropped rather than collapsed to a point.
        assert!(clamp_to_overlap(&[(0, 10)], 25.0, (2.0, 8.0)).is_empty());
    }

    #[test]
    fn levels_are_read_on_the_other_recordings_frames() {
        // b at 50fps, a at 25fps, b started 1s after a. b was loud in its frames
        // 50..100, which is 1s..2s of its own clock and 2s..3s of a's.
        let mut levels = vec![0u16; 200];
        levels[50..100].fill(9000);
        let clock = Clock { offset_seconds: 1.0, slope: 0.0 };
        let mapped = map_levels(&levels, 50.0, &clock, 100, 25.0);

        assert_eq!(mapped.len(), 100);
        // a frames 50..75 are 2s..3s.
        assert!(mapped[..50].iter().all(|&v| v == 0));
        assert!(mapped[50..75].iter().all(|&v| v == 9000));
        assert!(mapped[76..].iter().all(|&v| v == 0));
    }

    #[test]
    fn frames_before_the_second_recording_began_read_as_silence() {
        // Not as loud, and not as an index that wrapped round to the end.
        let levels = vec![9000u16; 100];
        let clock = Clock { offset_seconds: 4.0, slope: 0.0 };
        // b holds 4 seconds, so it covers a-times 4 through 8 and no further.
        let mapped = map_levels(&levels, 25.0, &clock, 250, 25.0);
        assert!(mapped[..100].iter().all(|&v| v == 0), "before b existed");
        assert!(mapped[100..199].iter().all(|&v| v == 9000), "while b ran");
        // Past the end of b, silence again rather than a panic.
        assert!(mapped[200..].iter().all(|&v| v == 0), "after b stopped");
    }

    #[test]
    fn shifted_segments_land_on_the_same_moments() {
        // a at 25fps, b at 30fps, b started 2s after a.
        let clock = Clock { offset_seconds: 2.0, slope: 0.0 };
        let shifted = shift_segments(&[(50, 100)], 25.0, 30.0, &clock, 10_000);
        // a frames 50..100 are 2s..4s, which on b is 0s..2s: frames 0..60.
        assert_eq!(shifted, vec![(0, 60)]);
        // The kept length survives the change of frame rate.
        let (start, end) = shifted[0];
        assert!(((end - start) as f64 / 30.0 - 2.0).abs() < 1e-9);
    }

    #[test]
    fn a_shifted_segment_cannot_run_off_either_end_of_the_second_file() {
        let clock = Clock { offset_seconds: 5.0, slope: 0.0 };
        // Before b existed: a 0..25 is b -5s..-4s, so nothing of it is there.
        assert!(shift_segments(&[(0, 25)], 25.0, 25.0, &clock, 1000).is_empty());
        // Past the end of b: clamped to what b has.
        assert_eq!(
            shift_segments(&[(250, 500)], 25.0, 25.0, &clock, 200),
            vec![(125, 200)]
        );
    }

    #[test]
    fn many_cuts_do_not_pull_the_pair_apart() {
        // Sixty ranges, 30fps against 25fps: the rates round differently every
        // time, which is exactly the case that used to accumulate.
        let segments: Vec<(usize, usize)> = (0..60)
            .map(|index| {
                // Past the 1.5s the second recording starts at, as holding the
                // edit to the overlap guarantees.
                let start = 60 + index * 137;
                (start, start + 61 + index % 7)
            })
            .collect();
        let clock = Clock { offset_seconds: 1.5, slope: 0.0 };
        let shifted = shift_segments(&segments, 30.0, 25.0, &clock, 1_000_000);
        assert_eq!(shifted.len(), segments.len());

        let kept_a: usize = segments.iter().map(|&(s, e)| e - s).sum();
        let kept_b: usize = shifted.iter().map(|&(s, e)| e - s).sum();
        let slip = (kept_b as f64 / 25.0) - (kept_a as f64 / 30.0);
        assert!(
            slip.abs() <= 1.0 / 25.0 / 2.0,
            "the two exports came out {slip:.4}s apart"
        );

        // And every range still begins on the moment it began in the first
        // recording, within the half frame its own grid allows.
        for (&(start, _), &(b_start, _)) in segments.iter().zip(shifted.iter()) {
            let wanted = clock.to_b(start as f64 / 30.0);
            let landed = b_start as f64 / 25.0;
            assert!(
                (landed - wanted).abs() <= 1.0 / 25.0 / 2.0 + 1e-9,
                "range at {start} landed {landed} instead of {wanted}"
            );
        }
    }

    #[test]
    fn the_running_total_catches_up_rather_than_compounding() {
        // A range whose length rounds down is followed by one that rounds up, so
        // the pair never walks away from itself. Ten 0.5s ranges at 30fps are 15
        // frames each; at 24fps they are 12, and the shared total is what decides
        // which of 12 and 13 each one gets.
        let segments: Vec<(usize, usize)> = (0..10).map(|i| (i * 60, i * 60 + 15)).collect();
        let clock = Clock { offset_seconds: 0.0, slope: 0.0 };
        let shifted = shift_segments(&segments, 30.0, 24.0, &clock, 1_000_000);
        let lengths: Vec<usize> = shifted.iter().map(|&(s, e)| e - s).collect();
        assert_eq!(lengths, vec![12, 12, 12, 12, 12, 12, 12, 12, 12, 12]);
        // 150 frames at 30fps is 5s; 120 at 24fps is 5s. Exactly, in this case.
        assert_eq!(lengths.iter().sum::<usize>(), 120);
    }

    #[test]
    fn drift_is_carried_into_the_far_end_of_a_long_pair() {
        // 200ppm over an hour is 720ms — twenty-odd frames out of step if the
        // shift were taken from the start of the recording.
        let clock = Clock { offset_seconds: 0.0, slope: 200e-6 };
        let near = shift_segments(&[(0, 25)], 25.0, 25.0, &clock, 1_000_000);
        let far = shift_segments(&[(90_000, 90_025)], 25.0, 25.0, &clock, 1_000_000);
        assert_eq!(near, vec![(0, 25)]);
        // An hour in, b is 0.72s behind: 18 frames at 25fps.
        assert_eq!(far, vec![(89_982, 90_007)]);
    }

    #[test]
    fn the_overlap_window_follows_the_lag() {
        // b sits 5 samples later on a's clock: a[5..] pairs with b[..5].
        assert_eq!(overlap_window(5, 10, 5), (5, 10));
        assert_eq!(overlap_at(5, 10, 5), 5);
        // b was rolling 3 samples earlier: a[..7] pairs with b[3..].
        assert_eq!(overlap_window(-3, 7, 10), (0, 7));
        // Pushed past each other entirely, nothing overlaps.
        assert_eq!(overlap_at(50, 10, 5), 0);
        assert_eq!(overlap_at(-50, 10, 5), 0);
    }
}
