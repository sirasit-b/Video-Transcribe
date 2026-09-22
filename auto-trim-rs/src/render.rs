//! Rendering the kept frames back out as a video.
//!
//! The picture is encoded in chunks that run in parallel, each ffmpeg decoding only
//! its own span of the source and filtering it down to the frames that survive with
//! `select`; output timestamps are rebuilt with `setpts` at the timeline's timebase,
//! so every chunk is constant-rate and starts at zero. The chunks are then
//! concatenated with a stream copy and muxed with the separately encoded audio.
//!
//! Why chunks rather than one pass:
//!
//! * A three-hour encode is the whole cost of a trim, and one ffmpeg does not scale
//!   across cores nearly as well as several do.
//! * The `select` expression is evaluated per frame, so a long video with thousands
//!   of cuts would spend real time walking one giant expression. Capping the
//!   segments per chunk keeps each expression short.
//!
//! Audio is *not* chunked: it is encoded once, from our own sample-exact splice, so
//! there is exactly one encoder priming block and no drift at the joins.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::jobs::{Job, WorkError, WorkResult};
use crate::probe::Rational;
use crate::proc::Ffmpeg;

/// Seconds of extra source decoded either side of a chunk, so the frame at the
/// chunk's first kept timestamp is definitely decoded rather than skipped by the
/// seek.
const CHUNK_LEAD: f64 = 0.5;

#[derive(Clone, Debug)]
pub struct RenderOptions {
    pub video_codec: String,
    /// Rate-control arguments for that encoder, as chosen by `encoder::Encoder`.
    pub video_quality: Vec<String>,
    /// Device setup and hardware decoding, which have to precede the input.
    pub video_input_args: Vec<String>,
    /// Appended to the filter chain, for an encoder that needs its frames moved
    /// onto the device first.
    pub video_filter_suffix: String,
    /// Unset when the frames stay on the GPU: the format is then the hardware's,
    /// and naming a software one is a contradiction ffmpeg rejects.
    pub pix_fmt: Option<String>,
    pub audio_codec: String,
    pub audio_bitrate: String,
    /// AAC coder: `fast` is twice the speed of ffmpeg's default at the same
    /// bitrate, which matters because audio is the long pole on a long video.
    pub audio_coder: String,
    /// Milliseconds of ramp at each end of a kept range, so a splice that lands
    /// mid-waveform does not click. 0 turns it off.
    pub audio_fade_ms: f64,
    pub threads_per_chunk: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Chunk {
    pub segments: Vec<(usize, usize)>,
    pub frames: usize,
    /// Where this chunk's ffmpeg starts reading the source, in seconds.
    pub read_from: f64,
    /// Where it stops reading, in seconds.
    pub read_to: f64,
}

/// Split the kept segments into consecutive chunks that can encode in parallel.
///
/// `workers` chunks is the target, but a chunk never holds more than
/// `max_segments_per_chunk` segments (which bounds the per-frame cost of its
/// `select` expression) and is never shorter than `min_chunk_seconds` of kept
/// footage unless a cap forces it (each extra chunk pays for its own seek and
/// decode lead-in, so tiny chunks are a loss).
///
/// A chunk also breaks before a gap longer than `max_gap_seconds`: a chunk decodes
/// its whole span, cut-away parts included, so carrying a long silence across a
/// chunk costs more than starting another one.
pub fn plan_chunks(
    segments: &[(usize, usize)],
    timebase: f64,
    workers: usize,
    max_segments_per_chunk: usize,
    min_chunk_seconds: f64,
    max_gap_seconds: f64,
) -> Vec<Chunk> {
    if segments.is_empty() {
        return Vec::new();
    }

    let max_segments = max_segments_per_chunk.max(1);
    let kept_frames: usize = segments.iter().map(|(start, end)| end - start).sum();
    let by_segments = segments.len().div_ceil(max_segments);
    let target_chunks = workers.max(1).max(by_segments).min(segments.len());
    let min_frames = (min_chunk_seconds * timebase).round().max(1.0) as usize;
    let target_frames = kept_frames.div_ceil(target_chunks).max(min_frames);

    let max_gap_frames = (max_gap_seconds.max(0.0) * timebase).round() as usize;
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut current: Vec<(usize, usize)> = Vec::new();
    let mut frames = 0usize;

    for &segment in segments {
        if let Some(&(_, previous_end)) = current.last() {
            if segment.0 - previous_end > max_gap_frames {
                chunks.push(build_chunk(std::mem::take(&mut current), frames, timebase));
                frames = 0;
            }
        }
        current.push(segment);
        frames += segment.1 - segment.0;
        if current.len() >= max_segments || frames >= target_frames {
            chunks.push(build_chunk(std::mem::take(&mut current), frames, timebase));
            frames = 0;
        }
    }
    if !current.is_empty() {
        chunks.push(build_chunk(current, frames, timebase));
    }

    chunks
}

fn build_chunk(segments: Vec<(usize, usize)>, frames: usize, timebase: f64) -> Chunk {
    let first = segments.first().expect("chunk has segments").0;
    let last = segments.last().expect("chunk has segments").1;
    Chunk {
        frames,
        read_from: (first as f64 / timebase - CHUNK_LEAD).max(0.0),
        read_to: last as f64 / timebase + CHUNK_LEAD,
        segments,
    }
}

/// One kept range, as a test on the frame's presentation time.
///
/// The half frame of slack centres each comparison between two frame times, so
/// floating point noise in either the source timestamps or the printed bounds
/// cannot pull a neighbouring frame in or push an edge frame out.
fn between_term(segment: (usize, usize), timebase: f64) -> String {
    let slack = 0.5 / timebase;
    let from = segment.0 as f64 / timebase - slack;
    let to = segment.1 as f64 / timebase - slack;
    format!("between(t\\,{from:.6}\\,{to:.6})")
}

/// The `select` expression for one chunk's kept frames: true when the frame falls
/// in any of them.
///
/// The terms are summed as a balanced tree rather than a flat `a+b+c+...` chain.
/// FFmpeg's expression parser descends recursively with a fixed budget (100
/// levels), and a flat chain spends that budget in proportion to the number of
/// terms — around 135 of them is enough to fail the parse outright with "Cannot
/// allocate memory". A balanced tree needs only log2(n) levels, so the expression
/// parses whatever the cut count. (Evaluation is still linear in the terms, which
/// is what `max_segments_per_chunk` bounds.)
fn select_expression(segments: &[(usize, usize)], timebase: f64) -> String {
    if segments.is_empty() {
        return "0".to_string();
    }
    if segments.len() == 1 {
        return between_term(segments[0], timebase);
    }
    let middle = segments.len() / 2;
    format!(
        "({}+{})",
        select_expression(&segments[..middle], timebase),
        select_expression(&segments[middle..], timebase)
    )
}

/// The whole filter graph for one chunk.
///
/// Passed to ffmpeg as an argument rather than through `-filter_complex_script`:
/// the segment cap keeps this to a few kilobytes, and the script option was
/// removed in ffmpeg 8, so inlining it works across versions.
pub fn filter_graph(segments: &[(usize, usize)], timebase: f64, suffix: &str) -> String {
    // setpts rebuilds a constant-rate clock at the timeline's timebase: output
    // frame N lands at N/timebase seconds, with no gaps where the cuts were.
    // Neither filter touches pixels, so both work on hardware frames — which is
    // what lets a GPU pipeline decode, cut and encode without a round trip
    // through system memory.
    format!(
        "[0:v]select='{}',setpts=N/TB/{:.10}{suffix}[v]",
        select_expression(segments, timebase),
        timebase
    )
}

fn is_mp4_like(output: &Path) -> bool {
    matches!(
        output
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .unwrap_or_default()
            .as_str(),
        "mp4" | "m4v" | "m4a" | "mov"
    )
}

/// Encode one chunk of the picture, reporting frames as ffmpeg finishes them.
///
/// Returns the file written and the number of frames it holds, as ffmpeg counted
/// them.
///
/// Each chunk decodes only its own span of the source, so the chunks together
/// decode the file about once however many of them there are.
#[allow(clippy::too_many_arguments)]
pub fn render_chunk(
    ffmpeg: &str,
    source: &Path,
    chunk: &Chunk,
    index: usize,
    timebase: Rational,
    options: &RenderOptions,
    work_dir: &Path,
    job_id: &str,
    job: &Job,
) -> WorkResult<(PathBuf, u64)> {
    let output = work_dir.join(format!("{job_id}-v{index}.mp4"));

    let mut command = Command::new(ffmpeg);
    command.args([
        "-v",
        "error",
        "-nostdin",
        "-y",
        // Progress on stdout is how the bar advances during the encode.
        "-progress",
        "pipe:1",
        "-nostats",
    ]);
    // Hardware decoding and device setup belong before the input.
    command.args(&options.video_input_args);
    command
        .args([
            // Keep source timestamps through the seek, so the select expression
            // can speak in absolute time regardless of where the chunk starts.
            "-copyts",
            "-ss",
            &format!("{:.6}", chunk.read_from),
            "-to",
            &format!("{:.6}", chunk.read_to),
            "-i",
        ])
        .arg(source)
        .arg("-filter_complex")
        .arg(filter_graph(
            &chunk.segments,
            timebase.as_f64(),
            &options.video_filter_suffix,
        ))
        .args(["-map", "[v]", "-an", "-sn", "-dn"])
        .args(["-c:v", &options.video_codec])
        .args(&options.video_quality);
    if let Some(format) = &options.pix_fmt {
        command.args(["-pix_fmt", format]);
    }
    command
        // Matches what setpts just wrote, so nothing gets duplicated or dropped.
        .args(["-r", &timebase.to_string(), "-fps_mode", "cfr"])
        .args(["-threads", &options.threads_per_chunk.to_string()])
        .arg(&output)
        .stdin(Stdio::null())
        .stdout(Stdio::piped());

    let mut process = Ffmpeg::spawn(&mut command, job).map_err(WorkError::Failed)?;
    let stdout = process.stdout.take().expect("stdout is piped");

    let mut frames = 0u64;
    let mut reported = 0u64;
    for line in BufReader::new(stdout).lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => break,
        };
        if let Some(value) = line.strip_prefix("frame=") {
            if let Ok(count) = value.trim().parse::<u64>() {
                frames = count;
                if frames > reported {
                    job.progress.add_rendered_frames(frames - reported);
                    reported = frames;
                }
            }
        }
    }

    process.wait(job)?;

    Ok((output, frames))
}

/// Join the chunks and mux them with the audio, copying every stream.
pub fn concat_and_mux(
    ffmpeg: &str,
    chunks: &[PathBuf],
    audio: &[PathBuf],
    output: &Path,
    work_dir: &Path,
    job_id: &str,
    job: &Job,
) -> WorkResult<()> {
    let mut command = Command::new(ffmpeg);
    command.args(["-v", "error", "-nostdin", "-y"]);

    let mut list_path: Option<PathBuf> = None;
    let mut audio_input_base = 0;
    if !chunks.is_empty() {
        let path = work_dir.join(format!("{job_id}-chunks.txt"));
        let mut list = String::new();
        for chunk in chunks {
            // Paths are ours: job ids and indexes, no quotes to escape.
            list.push_str(&format!("file '{}'\n", chunk.display()));
        }
        std::fs::write(&path, list)
            .map_err(|err| WorkError::Failed(format!("could not write the chunk list: {err}")))?;
        command.args(["-f", "concat", "-safe", "0", "-i"]).arg(&path);
        list_path = Some(path);
        audio_input_base = 1;
    }
    for track in audio {
        command.arg("-i").arg(track);
    }

    if !chunks.is_empty() {
        command.args(["-map", "0:v:0"]);
    }
    for index in 0..audio.len() {
        command.args(["-map", &format!("{}:a:0", audio_input_base + index)]);
    }
    command.args(["-c", "copy"]);
    if is_mp4_like(output) {
        command.args(["-movflags", "+faststart"]);
    }
    command.arg(output).stdin(Stdio::null());

    let process = Ffmpeg::spawn(&mut command, job).map_err(WorkError::Failed)?;
    let result = process.wait(job);
    if let Some(path) = list_path {
        let _ = std::fs::remove_file(path);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_expression_keeps_each_segment_between_frame_times() {
        // At 30fps a frame sits every 1/30s; frames 30..60 cover 1.0s to 2.0s,
        // and the bounds land half a frame before each, i.e. 0.983 and 1.983.
        let script = filter_graph(&[(30, 60)], 30.0, "");
        assert!(script.contains("between(t\\,0.983333\\,1.983333)"), "{script}");
        assert!(script.starts_with("[0:v]select='"), "{script}");
        assert!(script.contains("setpts=N/TB/30."), "{script}");
    }

    #[test]
    fn segments_are_summed_into_one_expression() {
        let script = filter_graph(&[(0, 3), (9, 12)], 30.0, "");
        assert_eq!(script.matches("between(").count(), 2);
        assert!(script.contains(")+between("), "{script}");
    }

    /// Deepest parenthesis nesting in the expression.
    fn nesting_depth(expression: &str) -> usize {
        let mut depth = 0usize;
        let mut deepest = 0usize;
        for character in expression.chars() {
            match character {
                '(' => {
                    depth += 1;
                    deepest = deepest.max(depth);
                }
                ')' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        deepest
    }

    #[test]
    fn many_segments_stay_shallow_enough_to_parse() {
        // FFmpeg's parser gives up past about 100 levels of recursion, and a flat
        // chain of terms hits that in the low hundreds of cuts. The tree has to
        // stay far below it even for a chunk packed with segments.
        let segments: Vec<(usize, usize)> = (0..2000).map(|i| (i * 10, i * 10 + 3)).collect();
        let expression = filter_graph(&segments, 30.0, "");
        assert_eq!(expression.matches("between(").count(), 2000);
        // between(t\,..) itself adds no parens beyond its own call.
        assert!(
            nesting_depth(&expression) <= 16,
            "depth {} is too deep",
            nesting_depth(&expression)
        );
    }

    #[test]
    fn every_segment_appears_exactly_once() {
        let segments = vec![(0, 30), (60, 90), (120, 150), (180, 210), (240, 270)];
        let expression = filter_graph(&segments, 30.0, "");
        for &(start, _) in &segments {
            let time = start as f64 / 30.0 - 0.5 / 30.0;
            let needle = format!("{time:.6}");
            assert_eq!(
                expression.matches(&needle).count(),
                1,
                "segment at {time} missing from {expression}"
            );
        }
    }

    #[test]
    fn an_empty_edit_selects_nothing() {
        assert!(filter_graph(&[], 30.0, "").contains("select='0'"));
    }

    #[test]
    fn a_device_filter_suffix_joins_the_chain() {
        // VAAPI from a CPU decode needs the frames uploaded at the end of the
        // chain, before the encoder sees them.
        let script = filter_graph(&[(0, 30)], 30.0, ",format=nv12,hwupload");
        assert!(script.ends_with(",format=nv12,hwupload[v]"), "{script}");
        assert!(script.contains("setpts=N/TB/30."), "{script}");
    }

    #[test]
    fn a_short_edit_stays_in_one_chunk() {
        // 10s of kept footage over 4 workers: splitting it four ways would cost
        // more in seeks than it saves.
        let segments = vec![(0, 150), (300, 450)];
        let chunks = plan_chunks(&segments, 30.0, 4, 400, 20.0, 3600.0);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].frames, 300);
    }

    #[test]
    fn long_edits_split_across_workers() {
        // 40 segments of 60s each: 40 minutes of footage, 4 workers.
        let segments: Vec<(usize, usize)> = (0..40)
            .map(|i| (i * 2000, i * 2000 + 1800))
            .collect();
        let chunks = plan_chunks(&segments, 30.0, 4, 400, 20.0, 3600.0);
        assert_eq!(chunks.len(), 4);
        let total: usize = chunks.iter().map(|chunk| chunk.frames).sum();
        assert_eq!(total, 40 * 1800);
        // Every segment lands in exactly one chunk, in order.
        let rejoined: Vec<(usize, usize)> = chunks
            .iter()
            .flat_map(|chunk| chunk.segments.clone())
            .collect();
        assert_eq!(rejoined, segments);
    }

    #[test]
    fn the_segment_cap_forces_extra_chunks() {
        // 1000 tiny segments with a cap of 100 must become 10 chunks, whatever
        // the worker count or the minimum chunk length say.
        let segments: Vec<(usize, usize)> = (0..1000).map(|i| (i * 10, i * 10 + 3)).collect();
        let chunks = plan_chunks(&segments, 30.0, 1, 100, 60.0, 3600.0);
        assert_eq!(chunks.len(), 10);
        for chunk in &chunks {
            assert!(chunk.segments.len() <= 100);
        }
    }

    #[test]
    fn chunks_read_a_little_either_side_of_their_frames() {
        let chunks = plan_chunks(&[(300, 600)], 30.0, 1, 400, 0.0, 3600.0);
        let chunk = &chunks[0];
        // 300/30 = 10s, minus the lead-in.
        assert!((chunk.read_from - 9.5).abs() < 1e-9);
        assert!((chunk.read_to - 20.5).abs() < 1e-9);
    }

    #[test]
    fn a_long_gap_breaks_the_chunk() {
        // Two minutes of dead air between the segments: decoding across it costs
        // more than starting a second chunk.
        let segments = vec![(0, 900), (4500, 5400)];
        let chunks = plan_chunks(&segments, 30.0, 1, 400, 20.0, 10.0);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].segments, vec![(0, 900)]);
        assert_eq!(chunks[1].segments, vec![(4500, 5400)]);

        // A gap under the limit stays in one chunk.
        let tight = vec![(0, 900), (1000, 1900)];
        assert_eq!(plan_chunks(&tight, 30.0, 1, 400, 20.0, 10.0).len(), 1);
    }

    #[test]
    fn the_first_chunk_never_seeks_before_zero() {
        let chunks = plan_chunks(&[(0, 90)], 30.0, 1, 400, 0.0, 3600.0);
        assert_eq!(chunks[0].read_from, 0.0);
    }
}
