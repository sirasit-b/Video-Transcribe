//! Audio loudness analysis and sample-exact splicing.
//!
//! auto-editor decodes audio with libav, resamples it to interleaved signed
//! 16-bit at the source rate, and takes one level per timeline frame: the loudest
//! sample in that frame's slice of audio (`src/analyze/audio.nim`). We get the same
//! samples out of `ffmpeg -f s16le` and do the same arithmetic, including the
//! frame-size rounding and the partial tail chunk, so the levels match value for
//! value.
//!
//! Nothing here holds a whole track in memory: a three-hour file is read in 256 KiB
//! chunks, and spliced audio goes straight into an encoder's stdin rather than
//! through a temp file (two hours of kept stereo PCM would be 1.4 GB).

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::sync_channel;

use crate::jobs::{Job, WorkError, WorkResult};
use crate::proc::Ffmpeg;

/// Quantization of auto-editor's `Unorm16`: levels and the threshold they are
/// compared against are both 16-bit fixed point, so a comparison here answers the
/// same way it does there.
pub fn to_unorm16(value: f32) -> u16 {
    if value.is_nan() {
        return 0;
    }
    let clamped = value.clamp(0.0, 1.0);
    (clamped * 65535.0 + 0.5) as u16
}

/// The per-frame audio slice, in samples, as auto-editor's `AudioIterator` sizes
/// it: `sample_rate / timebase` with the rounding error carried into the next
/// frame so the slices stay aligned to the clock over a long file.
struct ChunkPlan {
    exact: f64,
    max_buffer: usize,
    accumulated_error: f64,
}

impl ChunkPlan {
    fn new(sample_rate: u32, timebase: f64) -> Self {
        let exact = sample_rate as f64 / timebase;
        ChunkPlan {
            exact,
            max_buffer: exact.ceil().max(1.0) as usize,
            accumulated_error: 0.0,
        }
    }

    fn next_size(&mut self) -> usize {
        let size_with_error = self.exact + self.accumulated_error;
        let current = (size_with_error.round() as i64).clamp(0, self.max_buffer as i64) as usize;
        self.accumulated_error = size_with_error - current as f64;
        current
    }
}

pub struct StreamAnalysis {
    /// One level per timeline frame, as `Unorm16`.
    pub levels: Vec<u16>,
    /// Cumulative sample offsets of the frames, `levels.len() + 1` long, so frame
    /// `i` covers samples `boundaries[i] .. boundaries[i + 1]`.
    pub boundaries: Vec<u64>,
}

fn max_abs(samples: &[i16]) -> i16 {
    let mut peak: i16 = 0;
    for &sample in samples {
        // Saturating: -32768 clamps to 32767 rather than wrapping negative,
        // which is what the NEON/SSE paths in auto-editor do.
        let magnitude = sample.saturating_abs();
        if magnitude > peak {
            peak = magnitude;
        }
    }
    peak
}

fn decoder(ffmpeg: &str, path: &Path, ordinal: usize, job: &Job) -> Result<Ffmpeg, String> {
    let mut command = Command::new(ffmpeg);
    command
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args([
            "-map",
            &format!("0:a:{ordinal}"),
            "-vn",
            "-sn",
            "-dn",
            "-f",
            "s16le",
            "-acodec",
            "pcm_s16le",
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped());
    Ffmpeg::spawn(&mut command, job)
}

/// Decode one audio stream and return its per-frame levels.
pub fn analyze_stream(
    ffmpeg: &str,
    path: &Path,
    ordinal: usize,
    channels: usize,
    sample_rate: u32,
    timebase: f64,
    job: &Job,
) -> WorkResult<StreamAnalysis> {
    if channels == 0 || sample_rate == 0 {
        return Err(WorkError::Failed(format!(
            "audio stream {ordinal} has no decodable layout"
        )));
    }

    let mut plan = ChunkPlan::new(sample_rate, timebase);
    // auto-editor only reads a frame once its FIFO holds a whole one, so a short
    // tail is emitted by a separate final read rather than as a full frame.
    let gate = plan.max_buffer;

    let mut process = decoder(ffmpeg, path, ordinal, job)?;
    let mut stdout = process.stdout.take().expect("stdout is piped");

    let mut levels: Vec<u16> = Vec::new();
    let mut boundaries: Vec<u64> = vec![0];
    let mut fifo: Vec<i16> = Vec::with_capacity((gate + 65536) * channels);
    let mut consumed_samples = 0usize; // offset into `fifo` of the unread audio
    let mut total_frames = 0u64;
    let mut read_buffer = vec![0u8; 256 * 1024];
    let mut odd_byte: Option<u8> = None;

    let outcome = (|| -> WorkResult<()> {
        loop {
            if job.is_canceled() {
                return Err(WorkError::Canceled);
            }

            let read = stdout.read(&mut read_buffer).map_err(|err| {
                WorkError::Failed(format!("failed to read decoded audio: {err}"))
            })?;
            if read == 0 {
                break;
            }

            let mut bytes = &read_buffer[..read];
            if let Some(low) = odd_byte.take() {
                fifo.push(i16::from_le_bytes([low, bytes[0]]));
                bytes = &bytes[1..];
            }
            let pairs = bytes.len() - (bytes.len() % 2);
            fifo.extend(
                bytes[..pairs]
                    .chunks_exact(2)
                    .map(|pair| i16::from_le_bytes([pair[0], pair[1]])),
            );
            if pairs < bytes.len() {
                odd_byte = Some(bytes[pairs]);
            }

            let mut produced = 0u64;
            while (fifo.len() - consumed_samples) / channels >= gate {
                let size = plan.next_size();
                if size == 0 {
                    break;
                }
                let end = consumed_samples + size * channels;
                let peak = max_abs(&fifo[consumed_samples..end]);
                levels.push(to_unorm16(peak as f32 / 32767.0));
                total_frames += size as u64;
                boundaries.push(total_frames);
                consumed_samples = end;
                produced += 1;
            }

            if consumed_samples > 0 {
                fifo.drain(..consumed_samples);
                consumed_samples = 0;
            }
            if produced > 0 {
                job.progress.add_analyzed_frames(produced);
            }
        }

        // The tail is shorter than a whole frame; auto-editor still emits one level
        // for it, covering the last sliver of the timeline.
        let remaining = (fifo.len() - consumed_samples) / channels;
        if remaining > 0 {
            let size = plan.next_size().min(remaining);
            if size > 0 {
                let end = consumed_samples + size * channels;
                let peak = max_abs(&fifo[consumed_samples..end]);
                levels.push(to_unorm16(peak as f32 / 32767.0));
                total_frames += size as u64;
                boundaries.push(total_frames);
                job.progress.add_analyzed_frames(1);
            }
        }
        Ok(())
    })();

    match outcome {
        Ok(()) => {
            process.wait(job)?;
            Ok(StreamAnalysis { levels, boundaries })
        }
        Err(err) => {
            process.kill();
            process.discard(job);
            Err(err)
        }
    }
}

/// Decode one audio stream again, keep only the chosen sample ranges, and encode
/// the result straight into `output`.
///
/// Cutting the audio ourselves is what keeps the joins sample-exact: `aselect` can
/// only cut on packet boundaries (~21ms at 48kHz), and rounding every cut to a
/// packet would drift out of sync with the picture a little on every one of them —
/// seconds of drift across the hundreds of cuts a long video produces.
///
/// Three things run at once: the decoder, this thread splicing its output, and the
/// encoder. Feeding the encoder from its own thread is what lets them overlap;
/// doing it inline made the decoder idle while we wrote and the encoder idle while
/// we read.
#[allow(clippy::too_many_arguments)]
pub fn splice_to_encoder(
    ffmpeg: &str,
    path: &Path,
    ordinal: usize,
    channels: usize,
    sample_rate: u32,
    ranges: &[(u64, u64)],
    output: &Path,
    codec: &str,
    bitrate: &str,
    coder: &str,
    job: &Job,
) -> WorkResult<()> {
    let frame_bytes = (channels * 2) as u64;

    let mut decode = decoder(ffmpeg, path, ordinal, job)?;
    let mut stdout = decode.stdout.take().expect("stdout is piped");

    let mut command = Command::new(ffmpeg);
    command
        .args([
            "-v",
            "error",
            "-nostdin",
            "-y",
            // Raw PCM is demuxed in small packets by default; reading it in big
            // blocks cuts the per-packet overhead on a long track.
            "-blocksize",
            "4194304",
            "-f",
            "s16le",
            "-ar",
            &sample_rate.to_string(),
            "-ac",
            &channels.to_string(),
            "-i",
            "pipe:0",
            // How far the encoder has actually got. Counting what we feed it
            // instead runs ahead of the truth — the channel and the pipe in front
            // of it hold a couple of megabytes, so feeding finishes well before
            // encoding does, and a progress bar would stall at that number while
            // the encoder drained.
            "-progress",
            "pipe:1",
            "-nostats",
            "-c:a",
            codec,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    if !bitrate.is_empty() {
        command.args(["-b:a", bitrate]);
    }
    if codec == "aac" && !coder.is_empty() {
        // ffmpeg's default AAC coder (twoloop) manages about 60x realtime here,
        // where `fast` does 133x at the same bitrate. On a long video the audio
        // leg is otherwise the whole render's bottleneck.
        command.args(["-aac_coder", coder]);
    }
    command.args(["-threads", "1"]).arg(output);

    let mut encode = Ffmpeg::spawn(&mut command, job)
        .map_err(WorkError::Failed)
        .map_err(|err| {
            decode.kill();
            err
        })?;
    let encoder_stdin = encode.stdin.take().expect("stdin is piped");
    let encoder_progress = encode.stdout.take().expect("stdout is piped");

    /// Why the splice loop stopped.
    enum Stop {
        /// The decoder reached the end of the track.
        Eof,
        /// Every kept range is written; the rest of the file is of no interest.
        RangesDone,
    }

    // Scoped threads, so the helpers can borrow the job. Anything that would
    // leave a helper blocked is killed inside the scope: the progress reader only
    // ends when the encoder's stdout closes, so waiting for it after killing the
    // encoder would be the wrong way round.
    let result = std::thread::scope(|scope| -> WorkResult<Stop> {
        scope.spawn(|| {
            let mut reported = 0u64;
            for line in BufReader::new(encoder_progress).lines() {
                let Ok(line) = line else { break };
                if let Some(value) = line.strip_prefix("out_time_us=") {
                    if let Ok(microseconds) = value.trim().parse::<u64>() {
                        // Report in sample-frames, the unit the splice planned in.
                        let done = microseconds * sample_rate as u64 / 1_000_000;
                        if done > reported {
                            job.progress.add_audio_samples(done - reported);
                            reported = done;
                        }
                    }
                }
            }
        });

        let (sender, receiver) = sync_channel::<Vec<u8>>(8);
        let writer = scope.spawn(move || -> Result<(), String> {
            // Owning stdin here is what closes it when the channel ends, which is
            // how the encoder is told to flush and exit.
            let mut stdin = encoder_stdin;
            for buffer in receiver {
                stdin
                    .write_all(&buffer)
                    .map_err(|err| format!("could not feed the audio encoder: {err}"))?;
            }
            Ok(())
        });

        let mut read_buffer = vec![0u8; 256 * 1024];
        let mut position = 0u64; // byte offset into the decoded stream
        let mut range_index = 0usize;

        let stop = (|| -> WorkResult<Stop> {
            loop {
                if job.is_canceled() {
                    return Err(WorkError::Canceled);
                }
                let read = stdout.read(&mut read_buffer).map_err(|err| {
                    WorkError::Failed(format!("failed to read decoded audio: {err}"))
                })?;
                if read == 0 {
                    return Ok(Stop::Eof);
                }
                let chunk_start = position;
                let chunk_end = position + read as u64;
                position = chunk_end;

                while range_index < ranges.len() {
                    let (range_start, range_end) = ranges[range_index];
                    let start = range_start * frame_bytes;
                    let end = range_end * frame_bytes;
                    if start >= chunk_end {
                        break;
                    }
                    if end <= chunk_start {
                        range_index += 1;
                        continue;
                    }
                    let from = (start.max(chunk_start) - chunk_start) as usize;
                    let to = (end.min(chunk_end) - chunk_start) as usize;
                    sender
                        .send(read_buffer[from..to].to_vec())
                        .map_err(|_| {
                            WorkError::Failed("the audio encoder stopped reading".to_string())
                        })?;
                    if end <= chunk_end {
                        range_index += 1;
                    } else {
                        break;
                    }
                }

                // Everything left to keep is behind us; the rest of the decode is
                // waste.
                if range_index >= ranges.len() {
                    return Ok(Stop::RangesDone);
                }
            }
        })();

        drop(sender);
        let written = writer
            .join()
            .unwrap_or_else(|_| Err("the audio writer panicked".to_string()));
        let combined = match (stop, written) {
            (Err(err), _) => Err(err),
            (Ok(_), Err(message)) => Err(WorkError::Failed(message)),
            (Ok(stop), Ok(())) => Ok(stop),
        };

        match &combined {
            // Waiting on a decoder nobody is reading from would block on a full
            // pipe, and its kept audio is already inside the encoder.
            Ok(Stop::RangesDone) => decode.kill(),
            Err(_) => {
                decode.kill();
                encode.kill();
            }
            Ok(Stop::Eof) => {}
        }
        combined
    });

    match result {
        Ok(Stop::Eof) => {
            decode.wait(job)?;
            encode.wait(job)
        }
        Ok(Stop::RangesDone) => {
            decode.discard(job);
            encode.wait(job)
        }
        Err(err) => {
            decode.discard(job);
            encode.discard(job);
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unorm16_matches_auto_editors_quantization() {
        assert_eq!(to_unorm16(0.0), 0);
        assert_eq!(to_unorm16(1.0), 65535);
        // The default 4% threshold, as auto-editor stores it.
        assert_eq!(to_unorm16(0.04), 2621);
        assert_eq!(to_unorm16(-1.0), 0);
        assert_eq!(to_unorm16(2.0), 65535);
        assert_eq!(to_unorm16(f32::NAN), 0);
    }

    #[test]
    fn frame_sizes_carry_their_rounding_error() {
        // 48000 / 29.97 = 1601.6 samples per frame: sizes alternate so the drift
        // never accumulates.
        let mut plan = ChunkPlan::new(48000, 30000.0 / 1001.0);
        let sizes: Vec<usize> = (0..5).map(|_| plan.next_size()).collect();
        assert_eq!(sizes.iter().sum::<usize>(), 8008);
        for size in sizes {
            assert!(size == 1601 || size == 1602, "unexpected frame size {size}");
        }
    }

    #[test]
    fn a_whole_rate_needs_no_correction() {
        let mut plan = ChunkPlan::new(48000, 30.0);
        assert_eq!(plan.next_size(), 1600);
        assert_eq!(plan.next_size(), 1600);
    }

    #[test]
    fn frame_sizes_stay_aligned_over_three_hours() {
        // 3h at 29.97fps is 323,676 frames. The carried error has to keep the
        // audio clock on the picture clock over all of them.
        let timebase = 30000.0 / 1001.0;
        let mut plan = ChunkPlan::new(48000, timebase);
        let frames = 323_676u64;
        let total: u64 = (0..frames).map(|_| plan.next_size() as u64).sum();
        let expected = (frames as f64 * 48000.0 / timebase).round() as u64;
        assert!(
            total.abs_diff(expected) <= 1,
            "drifted by {} samples",
            total.abs_diff(expected)
        );
    }

    #[test]
    fn peaks_saturate_instead_of_wrapping() {
        assert_eq!(max_abs(&[0, -5, 3]), 5);
        assert_eq!(max_abs(&[i16::MIN]), 32767);
        assert_eq!(max_abs(&[]), 0);
    }
}
