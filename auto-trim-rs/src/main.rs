//! auto-trim: silence-based automatic video trimming, in the shape auto-editor
//! does it.
//!
//! The edit decision — one audio level per timeline frame, a threshold, a margin
//! around every active run, and the short-run smoothing pass — is ported from
//! auto-editor so the two agree on what to keep. See `mask.rs` and `audio.rs`.
//!
//! Work runs as a job: `POST /jobs` starts one and returns immediately, `GET
//! /jobs/{id}` reports phase, progress and ETA, `DELETE /jobs/{id}` cancels. A
//! three-hour video takes minutes to render, which is far too long to hold a
//! request open for, and several jobs can be in flight at once — encode slots are
//! shared between them so the machine stays busy without thrashing.

mod audio;
mod cache;
mod encoder;
mod jobs;
mod mask;
mod probe;
mod proc;
mod render;

use std::{
    env,
    net::SocketAddr,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    extract::{Path as UrlPath, State},
    http::StatusCode,
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use jobs::{Job, Jobs, Mode, Outcome, Phase, PhaseWeights, Slots, WorkError, WorkResult};
use probe::{MediaInfo, Rational};

struct AppState {
    ffmpeg: String,
    ffprobe: String,
    video_root: PathBuf,
    work_dir: PathBuf,
    jobs: Jobs,
    /// Encode slots, shared by every job on this machine.
    slots: Slots,
    workers: usize,
    threads_per_chunk: usize,
    max_segments_per_chunk: usize,
    min_chunk_seconds: f64,
    max_gap_seconds: f64,
    max_duration: f64,
    /// The encoder chosen at startup, after testing what this machine can
    /// actually run.
    encoder: encoder::Encoder,
    /// What that test found: the CPU, the devices, and every encoder tried.
    capabilities: encoder::Capabilities,
    /// Audio levels from earlier runs, so changing a threshold does not re-read
    /// the whole file.
    levels: cache::Cache,
    calibration: Calibration,
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

fn default_threshold() -> f32 {
    0.04
}

fn default_margin() -> f64 {
    0.2
}

fn default_mincut() -> f64 {
    0.2
}

fn default_minclip() -> f64 {
    0.1
}

/// The editing options, defaulted to auto-editor's own defaults: `--edit audio`
/// at a 4% threshold, `--margin 0.2s`, `--smooth 0.2s,0.1s`.
#[derive(Clone, Deserialize)]
struct EditRequest {
    video_path: String,
    #[serde(default = "default_threshold")]
    threshold: f32,
    #[serde(default = "default_margin")]
    margin_start: f64,
    #[serde(default = "default_margin")]
    margin_end: f64,
    #[serde(default = "default_mincut")]
    mincut: f64,
    #[serde(default = "default_minclip")]
    minclip: f64,
    /// Cap on the segment list in the response; the counts and durations always
    /// describe the whole edit.
    #[serde(default)]
    max_segments_returned: Option<usize>,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum RequestMode {
    Analyze,
    Trim,
}

#[derive(Clone, Deserialize)]
struct JobRequest {
    #[serde(flatten)]
    edit: EditRequest,
    mode: RequestMode,
    /// Required for `trim`.
    #[serde(default)]
    output_path: Option<String>,
    #[serde(default)]
    video_codec: Option<String>,
    #[serde(default)]
    preset: Option<String>,
    #[serde(default)]
    crf: Option<i32>,
    #[serde(default)]
    pix_fmt: Option<String>,
    #[serde(default)]
    audio_codec: Option<String>,
    #[serde(default)]
    audio_bitrate: Option<String>,
    #[serde(default)]
    audio_coder: Option<String>,
}

#[derive(Deserialize)]
struct EstimateRequest {
    video_path: String,
    /// Share of the video assumed to survive the edit, for an estimate made
    /// before anything has been analyzed. 1.0 (the default) is the upper bound.
    #[serde(default)]
    assumed_kept_ratio: Option<f64>,
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct Segment {
    start_frame: usize,
    end_frame: usize,
    start: f64,
    end: f64,
}

#[derive(Serialize)]
struct Analysis {
    timebase: Rational,
    fps: f64,
    threshold: f32,
    margin_start: f64,
    margin_end: f64,
    mincut: f64,
    minclip: f64,
    margin_start_frames: i64,
    margin_end_frames: i64,
    mincut_frames: i64,
    minclip_frames: i64,
    audio_streams: usize,
    total_frames: usize,
    kept_frames: usize,
    removed_frames: usize,
    /// Duration the timeline covers (frames / timebase), which is what the cut
    /// percentages are measured against.
    timeline_duration: f64,
    /// Duration the container reports for the source file.
    source_duration: f64,
    output_duration: f64,
    removed_duration: f64,
    removed_ratio: f64,
    segment_count: usize,
    segments: Vec<Segment>,
    segments_truncated: bool,
    analyze_seconds: f64,
}

#[derive(Clone, Serialize)]
struct Estimate {
    /// Seconds each phase is expected to take.
    analyze_seconds: f64,
    render_seconds: f64,
    audio_seconds: f64,
    finish_seconds: f64,
    total_seconds: f64,
    /// True while the kept length is a guess rather than a measured figure.
    is_upper_bound: bool,
    assumed_kept_ratio: f64,
    duration: f64,
    kept_duration: f64,
    width: u32,
    height: u32,
    fps: f64,
    audio_streams: usize,
    workers: usize,
    /// The rates the estimate came from, which calibrate themselves as jobs run.
    analyze_rate: f64,
    audio_rate: f64,
    pixel_rate: f64,
}

#[derive(Serialize)]
struct JobStatus {
    job_id: String,
    mode: &'static str,
    state: &'static str,
    phase: &'static str,
    progress: f64,
    elapsed_seconds: f64,
    eta_seconds: Option<f64>,
    video_path: String,
    estimate: Option<serde_json::Value>,
    analysis: Option<serde_json::Value>,
    result: Option<serde_json::Value>,
    error: Option<String>,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    workers: usize,
    max_duration: f64,
    video_encoder: String,
    hardware_encoder: bool,
    pipeline: String,
}

// ---------------------------------------------------------------------------
// Estimation
// ---------------------------------------------------------------------------

/// Throughput rates, kept as running averages so estimates match the machine the
/// service actually runs on. Seeded from the environment, updated after every job;
/// in memory only, so a restart falls back to the seeds.
struct Calibration {
    /// Source seconds of audio analyzed per wall-clock second, per stream.
    analyze_rate: Mutex<f64>,
    /// Kept seconds of audio spliced and encoded per wall-clock second.
    audio_rate: Mutex<f64>,
    /// Pixels of picture encoded per wall-clock second, across all workers.
    pixel_rate: Mutex<f64>,
}

impl Calibration {
    fn from_env(seed: f64) -> Self {
        Calibration {
            analyze_rate: Mutex::new(env_f64("ANALYZE_RATE", 60.0)),
            audio_rate: Mutex::new(env_f64("AUDIO_RATE", 80.0)),
            // What the chosen encoder should manage, give or take. The seed only
            // has to be in the right ballpark — measurements take over after the
            // first render — but it is what the very first estimate shows, and a
            // GPU is an order of magnitude away from a CPU.
            pixel_rate: Mutex::new(env_f64("PIXEL_RATE", seed)),
        }
    }

    fn read(slot: &Mutex<f64>) -> f64 {
        slot.lock().map(|value| *value).unwrap_or(1.0)
    }

    /// Fold a measurement into the average. Short runs are ignored: process
    /// startup dominates them, and they would drag the rate down for real work.
    fn observe(slot: &Mutex<f64>, amount: f64, elapsed: f64, floor: f64, ceiling: f64) {
        if elapsed < 1.0 || amount <= 0.0 {
            return;
        }
        let measured = (amount / elapsed).clamp(floor, ceiling);
        if let Ok(mut value) = slot.lock() {
            *value = 0.7 * *value + 0.3 * measured;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn estimate(
        &self,
        info: &MediaInfo,
        timebase: f64,
        kept_duration: f64,
        assumed_kept_ratio: Option<f64>,
        trim: bool,
        workers: usize,
    ) -> Estimate {
        let analyze_rate = Self::read(&self.analyze_rate);
        let audio_rate = Self::read(&self.audio_rate);
        let pixel_rate = Self::read(&self.pixel_rate);

        let streams = info.audio.len().max(1);
        let analyze_seconds = info.duration * streams as f64 / analyze_rate;

        let (render_seconds, audio_seconds, finish_seconds) = if trim {
            let pixels = if info.has_video {
                kept_duration * timebase * info.width as f64 * info.height as f64
            } else {
                0.0
            };
            (
                pixels / pixel_rate,
                kept_duration * streams as f64 / audio_rate,
                1.0 + kept_duration / 600.0,
            )
        } else {
            (0.0, 0.0, 0.0)
        };

        // Audio and picture encode at the same time, so the render phase takes
        // however long the slower of the two needs.
        let total_seconds = analyze_seconds + render_seconds.max(audio_seconds) + finish_seconds;

        Estimate {
            analyze_seconds,
            render_seconds,
            audio_seconds,
            finish_seconds,
            total_seconds,
            is_upper_bound: assumed_kept_ratio.is_some(),
            assumed_kept_ratio: assumed_kept_ratio.unwrap_or(1.0),
            duration: info.duration,
            kept_duration,
            width: info.width,
            height: info.height,
            fps: timebase,
            audio_streams: info.audio.len(),
            workers,
            analyze_rate,
            audio_rate,
            pixel_rate,
        }
    }
}

impl Estimate {
    fn weights(&self) -> PhaseWeights {
        PhaseWeights {
            // A phase that cannot take zero time must not have zero weight, or
            // the bar would jump straight past it.
            analyze: self.analyze_seconds.max(0.01),
            render: self.render_seconds.max(self.audio_seconds),
            finish: self.finish_seconds,
        }
    }
}

fn env_f64(name: &str, default: f64) -> f64 {
    env::var(name)
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| *value > 0.0)
        .unwrap_or(default)
}

fn env_usize(name: &str, default: usize) -> usize {
    env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let host = env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
    let port = env::var("PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(8082);

    let cores = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(4);
    // Two threads per encode is the sweet spot for x264: more threads per process
    // buys less than another process does.
    let threads_per_chunk = env_usize("THREADS_PER_CHUNK", 2);
    let workers = env_usize("ENCODE_WORKERS", (cores / threads_per_chunk).max(1));

    let work_dir =
        PathBuf::from(env::var("WORK_DIR").unwrap_or_else(|_| "/tmp/auto-trim".to_string()));
    if let Err(err) = std::fs::create_dir_all(&work_dir) {
        eprintln!("could not create WORK_DIR {}: {err}", work_dir.display());
    }

    let work_dir_for_cache = work_dir.clone();

    // Find out what this machine can encode with before anything else: the answer
    // sets the estimate's starting rate as well as the renderer's arguments.
    let (detected_encoder, detected_capabilities) = encoder::detect(
        &env::var("FFMPEG_PATH").unwrap_or_else(|_| "ffmpeg".to_string()),
        &work_dir,
        env::var("VIDEO_CODEC").ok().as_deref(),
        &env::var("PRESET").unwrap_or_else(|_| "veryfast".to_string()),
        env_usize("CRF", 20) as i32,
        workers * threads_per_chunk,
    );
    let encoder_seed = detected_encoder.pixel_rate_seed;

    let state = Arc::new(AppState {
        ffmpeg: env::var("FFMPEG_PATH").unwrap_or_else(|_| "ffmpeg".to_string()),
        ffprobe: env::var("FFPROBE_PATH").unwrap_or_else(|_| "ffprobe".to_string()),
        video_root: canonicalize_existing(
            &env::var("VIDEO_ROOT").unwrap_or_else(|_| "/app/videos".to_string()),
        ),
        work_dir,
        jobs: Jobs::new(
            Duration::from_secs(env_usize("JOB_RETENTION_SECONDS", 7200) as u64),
            env_usize("MAX_ACTIVE_JOBS", 4),
        ),
        slots: Slots::new(workers),
        workers,
        threads_per_chunk,
        max_segments_per_chunk: env_usize("MAX_SEGMENTS_PER_CHUNK", 64),
        min_chunk_seconds: env_f64("MIN_CHUNK_SECONDS", 20.0),
        max_gap_seconds: env_f64("MAX_GAP_SECONDS", 10.0),
        // Three hours, as the product supports. A longer file is refused with a
        // clear reason rather than running for an unbounded time.
        max_duration: env_f64("MAX_DURATION_SECONDS", 10800.0),
        encoder: detected_encoder,
        capabilities: detected_capabilities,
        levels: cache::Cache::new(
            if env::var("LEVEL_CACHE").as_deref() == Ok("0") {
                None
            } else {
                Some(work_dir_for_cache.join("levels"))
            },
            (env_f64("LEVEL_CACHE_MB", 2048.0) * 1024.0 * 1024.0) as u64,
        ),
        calibration: Calibration::from_env(encoder_seed),
    });

    let app = Router::new()
        .route("/health", get(health))
        .route("/capabilities", get(capabilities))
        .route("/estimate", post(estimate))
        .route("/jobs", post(start_job))
        .route("/jobs/:job_id", get(job_status))
        .route("/jobs/:job_id", delete(cancel_job))
        .with_state(state);

    let addr: SocketAddr = format!("{host}:{port}").parse().expect("invalid bind address");
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind tcp listener");

    axum::serve(listener, app).await.expect("server failed");
}

async fn health(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        workers: state.workers,
        max_duration: state.max_duration,
        video_encoder: state.encoder.name.clone(),
        hardware_encoder: state.encoder.hardware,
        pipeline: state.encoder.pipeline.clone(),
    })
}

/// What this machine can encode with, and what was tried to find out.
async fn capabilities(State(state): State<Arc<AppState>>) -> Json<encoder::Capabilities> {
    Json(state.capabilities.clone())
}

/// A failure with the status it should be reported as: anything the caller can fix
/// (a bad path, a video with no audio, one that is too long) is a 4xx, a broken
/// ffmpeg run is a 500.
struct Problem {
    status: StatusCode,
    message: String,
}

impl Problem {
    fn into_parts(self) -> (StatusCode, String) {
        (self.status, self.message)
    }
}

fn bad_request(message: impl Into<String>) -> Problem {
    Problem {
        status: StatusCode::BAD_REQUEST,
        message: message.into(),
    }
}

fn failed(message: impl Into<String>) -> Problem {
    Problem {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: message.into(),
    }
}

/// What a probe established about a source, before any decoding.
struct Source {
    path: PathBuf,
    info: MediaInfo,
    timebase: Rational,
}

fn open_source(state: &AppState, video_path: &str) -> Result<Source, Problem> {
    let path = resolve_existing_within(&state.video_root, video_path).map_err(bad_request)?;
    let info = probe::probe(&state.ffprobe, &path).map_err(failed)?;

    if info.audio.is_empty() {
        return Err(bad_request(
            "this file has no audio track, so there is no silence to detect",
        ));
    }
    if info.duration > state.max_duration {
        let hours = state.max_duration / 3600.0;
        return Err(bad_request(format!(
            "this video is {:.0} minutes long; auto trim supports up to {hours:.0} hours",
            info.duration / 60.0
        )));
    }

    let timebase = probe::sane_timebase(info.frame_rate);
    Ok(Source {
        path,
        info,
        timebase,
    })
}

async fn estimate(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<EstimateRequest>,
) -> Result<Json<Estimate>, (StatusCode, String)> {
    tokio::task::spawn_blocking(move || {
        let source = open_source(&state, &payload.video_path)?;
        let ratio = payload.assumed_kept_ratio.unwrap_or(1.0).clamp(0.0, 1.0);
        Ok(Json(state.calibration.estimate(
            &source.info,
            source.timebase.as_f64(),
            source.info.duration * ratio,
            Some(ratio),
            true,
            state.workers,
        )))
    })
    .await
    .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?
    .map_err(Problem::into_parts)
}

async fn start_job(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<JobRequest>,
) -> Result<(StatusCode, Json<JobStatus>), (StatusCode, String)> {
    let status = tokio::task::spawn_blocking(move || launch(state, payload))
        .await
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?
        .map_err(Problem::into_parts)?;
    Ok((StatusCode::ACCEPTED, Json(status)))
}

async fn job_status(
    State(state): State<Arc<AppState>>,
    UrlPath(job_id): UrlPath<String>,
) -> Result<Json<JobStatus>, (StatusCode, String)> {
    let job = state
        .jobs
        .get(&job_id)
        .ok_or((StatusCode::NOT_FOUND, "no such job".to_string()))?;
    Ok(Json(status_of(&job)))
}

async fn cancel_job(
    State(state): State<Arc<AppState>>,
    UrlPath(job_id): UrlPath<String>,
) -> Result<Json<JobStatus>, (StatusCode, String)> {
    let job = state
        .jobs
        .get(&job_id)
        .ok_or((StatusCode::NOT_FOUND, "no such job".to_string()))?;
    job.cancel();
    Ok(Json(status_of(&job)))
}

fn status_of(job: &Arc<Job>) -> JobStatus {
    let (state, error, result) = job.snapshot();
    let progress = if state == "done" {
        1.0
    } else {
        job.progress.fraction()
    };
    let elapsed = job.elapsed();
    let estimate = job.estimate.lock().ok().and_then(|slot| slot.clone());

    // Measured pace as soon as there is enough of it to mean anything; the
    // up-front estimate until then.
    let eta = match state {
        "running" | "canceling" => {
            if progress > 0.05 {
                Some((elapsed * (1.0 - progress) / progress).max(0.0))
            } else {
                estimate
                    .as_ref()
                    .and_then(|value| value.get("total_seconds").and_then(|v| v.as_f64()))
                    .map(|total| (total - elapsed).max(0.0))
            }
        }
        _ => None,
    };

    JobStatus {
        job_id: job.id.clone(),
        mode: job.mode.as_str(),
        state,
        phase: job.progress.phase().as_str(),
        progress,
        elapsed_seconds: elapsed,
        eta_seconds: eta,
        video_path: job.video_path.clone(),
        estimate,
        analysis: job.analysis.lock().ok().and_then(|slot| slot.clone()),
        result,
        error,
    }
}

/// Validate the request, register the job, and hand the work to a thread.
fn launch(state: Arc<AppState>, request: JobRequest) -> Result<JobStatus, Problem> {
    if !(0.0..=1.0).contains(&request.edit.threshold) {
        return Err(bad_request("threshold must be between 0 and 1"));
    }
    for (name, value) in [
        ("margin_start", request.edit.margin_start),
        ("margin_end", request.edit.margin_end),
        ("mincut", request.edit.mincut),
        ("minclip", request.edit.minclip),
    ] {
        if !value.is_finite() || value.abs() > 3600.0 {
            return Err(bad_request(format!(
                "{name} must be a length within 3600 seconds"
            )));
        }
    }

    let trim = request.mode == RequestMode::Trim;
    let output = if trim {
        let requested = request
            .output_path
            .as_deref()
            .filter(|path| !path.is_empty())
            .ok_or_else(|| bad_request("output_path is required to trim"))?;
        Some(resolve_output_within(&state.video_root, requested).map_err(bad_request)?)
    } else {
        None
    };

    // Probing here means a file that cannot be trimmed at all is refused now,
    // with a reason, instead of becoming a job that fails a moment later.
    let source = open_source(&state, &request.edit.video_path)?;
    let estimate = state.calibration.estimate(
        &source.info,
        source.timebase.as_f64(),
        source.info.duration,
        Some(1.0),
        trim,
        state.workers,
    );

    let mode = if trim { Mode::Trim } else { Mode::Analyze };
    let job = state
        .jobs
        .create(mode, source.path.to_string_lossy().into_owned())
        .map_err(|active| Problem {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: format!(
                "{active} trim jobs are already running; try again when one finishes"
            ),
        })?;

    job.progress.set_weights(estimate.weights());
    job.progress.set_phase(Phase::Analyzing);
    job.progress.set_expected_frames(
        (source.info.duration * source.timebase.as_f64()).round() as u64
            * source.info.audio.len().max(1) as u64,
    );
    if let Ok(mut slot) = job.estimate.lock() {
        *slot = serde_json::to_value(&estimate).ok();
    }

    let status = status_of(&job);
    let worker_state = Arc::clone(&state);
    let worker_job = Arc::clone(&job);
    std::thread::spawn(move || {
        let outcome = run_job(&worker_state, &worker_job, &request, source, output);
        worker_job.finish(match outcome {
            Ok(value) => Outcome::Done(value),
            Err(WorkError::Canceled) => Outcome::Canceled,
            Err(WorkError::Failed(message)) => Outcome::Failed(message),
        });
    });

    Ok(status)
}

// ---------------------------------------------------------------------------
// The work
// ---------------------------------------------------------------------------

/// Frames for a duration in seconds, the way auto-editor converts `--margin` and
/// `--smooth`: the requested length scaled by the timebase, then rounded.
fn to_frames(seconds: f64, timebase: f64) -> i64 {
    (seconds * timebase).round() as i64
}

struct Decision {
    streams: Vec<audio::StreamAnalysis>,
    segments: Vec<(usize, usize)>,
    kept_frames: usize,
    analysis: Analysis,
}

fn analyze(
    state: &AppState,
    job: &Job,
    request: &EditRequest,
    source: &Source,
) -> WorkResult<Decision> {
    let started = Instant::now();
    let timebase = source.timebase.as_f64();
    let path = &source.path;

    // One decoder per audio track, in parallel: `--edit audio` analyzes every
    // stream and a frame survives if any one of them is loud enough.
    let streams: Vec<audio::StreamAnalysis> = std::thread::scope(|scope| {
        let handles: Vec<_> = source
            .info
            .audio
            .iter()
            .map(|stream| {
                scope.spawn(move || {
                    let key = cache::Key {
                        source: path.clone(),
                        timebase: source.timebase,
                        ordinal: stream.ordinal,
                        sample_rate: stream.sample_rate,
                        channels: stream.channels,
                    };
                    if let Some(cached) = state.levels.load(&key) {
                        // The bar still has to account for the frames this pass
                        // would have reported.
                        job.progress.add_analyzed_frames(cached.levels.len() as u64);
                        return Ok(cached);
                    }

                    let analysis = audio::analyze_stream(
                        &state.ffmpeg,
                        path,
                        stream.ordinal,
                        stream.channels,
                        stream.sample_rate,
                        timebase,
                        job,
                    )?;
                    state.levels.store(&key, &analysis);
                    Ok(analysis)
                })
            })
            .collect();

        let mut analyses = Vec::new();
        let mut failure: Option<WorkError> = None;
        for handle in handles {
            match handle.join() {
                Ok(Ok(analysis)) => analyses.push(analysis),
                Ok(Err(err)) => failure = Some(failure.unwrap_or(err)),
                Err(_) => {
                    failure = Some(
                        failure.unwrap_or(WorkError::Failed("audio analysis panicked".to_string())),
                    )
                }
            }
        }
        match failure {
            Some(err) => Err(err),
            None => Ok(analyses),
        }
    })?;

    let threshold = audio::to_unorm16(request.threshold);
    let total_frames = streams
        .iter()
        .map(|stream| stream.levels.len())
        .max()
        .unwrap_or(0);
    let mut active = vec![false; total_frames];
    for stream in &streams {
        for (index, &level) in stream.levels.iter().enumerate() {
            if level >= threshold {
                active[index] = true;
            }
        }
    }

    let margin_frames = (
        to_frames(request.margin_start, timebase),
        to_frames(request.margin_end, timebase),
    );
    let smooth_frames = (
        to_frames(request.mincut, timebase),
        to_frames(request.minclip, timebase),
    );

    mask::mut_margin(&mut active, margin_frames.0, margin_frames.1);
    mask::smoothing(&mut active, smooth_frames.0, smooth_frames.1);

    let segments = mask::active_segments(&active);
    let kept_frames: usize = segments.iter().map(|&(start, end)| end - start).sum();
    let removed_frames = total_frames - kept_frames;

    let limit = request.max_segments_returned.unwrap_or(usize::MAX);
    let analysis = Analysis {
        timebase: source.timebase,
        fps: timebase,
        threshold: request.threshold,
        margin_start: request.margin_start,
        margin_end: request.margin_end,
        mincut: request.mincut,
        minclip: request.minclip,
        margin_start_frames: margin_frames.0,
        margin_end_frames: margin_frames.1,
        mincut_frames: smooth_frames.0,
        minclip_frames: smooth_frames.1,
        audio_streams: streams.len(),
        total_frames,
        kept_frames,
        removed_frames,
        timeline_duration: total_frames as f64 / timebase,
        source_duration: source.info.duration,
        output_duration: kept_frames as f64 / timebase,
        removed_duration: removed_frames as f64 / timebase,
        removed_ratio: if total_frames == 0 {
            0.0
        } else {
            removed_frames as f64 / total_frames as f64
        },
        segment_count: segments.len(),
        segments: segments
            .iter()
            .take(limit)
            .map(|&(start, end)| Segment {
                start_frame: start,
                end_frame: end,
                start: start as f64 / timebase,
                end: end as f64 / timebase,
            })
            .collect(),
        segments_truncated: segments.len() > limit,
        analyze_seconds: round2(started.elapsed().as_secs_f64()),
    };

    Calibration::observe(
        &state.calibration.analyze_rate,
        source.info.duration * source.info.audio.len().max(1) as f64,
        started.elapsed().as_secs_f64(),
        1.0,
        10_000.0,
    );

    Ok(Decision {
        streams,
        segments,
        kept_frames,
        analysis,
    })
}

fn run_job(
    state: &AppState,
    job: &Job,
    request: &JobRequest,
    source: Source,
    output: Option<PathBuf>,
) -> WorkResult<serde_json::Value> {
    let decision = analyze(state, job, &request.edit, &source)?;
    let analysis_value = serde_json::to_value(&decision.analysis)
        .map_err(|err| WorkError::Failed(err.to_string()))?;
    if let Ok(mut slot) = job.analysis.lock() {
        *slot = Some(analysis_value.clone());
    }

    let output = match output {
        Some(path) => path,
        // An analyze job is done as soon as the decision is made.
        None => return Ok(analysis_value),
    };

    if decision.segments.is_empty() {
        return Err(WorkError::Failed(
            "every frame reads as silence at this threshold, so there would be nothing \
             left to keep"
                .to_string(),
        ));
    }

    // Now that the kept length is known, re-weight the bar and re-estimate.
    let timebase = source.timebase.as_f64();
    let kept_duration = decision.kept_frames as f64 / timebase;
    let estimate = state.calibration.estimate(
        &source.info,
        timebase,
        kept_duration,
        None,
        true,
        state.workers,
    );
    job.progress.set_weights(PhaseWeights {
        analyze: decision.analysis.analyze_seconds.max(0.01),
        render: estimate.render_seconds.max(estimate.audio_seconds),
        finish: estimate.finish_seconds,
    });
    if let Ok(mut slot) = job.estimate.lock() {
        *slot = serde_json::to_value(&estimate).ok();
    }
    job.progress.set_kept_frames(decision.kept_frames as u64);
    job.progress.set_phase(Phase::Rendering);

    // A request that names its own encoder or quality gets one resolved for it;
    // otherwise the startup choice stands.
    let encoder = match (&request.video_codec, &request.preset, request.crf) {
        (None, None, None) => state.encoder.clone(),
        (codec, preset, crf) => {
            encoder::detect(
                &state.ffmpeg,
                &state.work_dir,
                codec.as_deref().or(Some(state.encoder.name.as_str())),
                preset.as_deref().unwrap_or("veryfast"),
                crf.unwrap_or(20),
                state.workers * state.threads_per_chunk,
            )
            .0
        }
    };

    let encoder_is_hardware = encoder.hardware;
    let encoder_pipeline = encoder.pipeline.clone();
    let options = render::RenderOptions {
        video_codec: encoder.name,
        video_quality: encoder.quality,
        video_input_args: encoder.input_args,
        video_filter_suffix: encoder.filter_suffix,
        // A caller-named format wins; otherwise the encoder decides, and a
        // hardware pipeline leaves it unset.
        pix_fmt: request.pix_fmt.clone().or(encoder.pix_fmt),
        audio_codec: request
            .audio_codec
            .clone()
            .unwrap_or_else(|| "aac".to_string()),
        audio_bitrate: request
            .audio_bitrate
            .clone()
            .unwrap_or_else(|| "192k".to_string()),
        audio_coder: request
            .audio_coder
            .clone()
            .unwrap_or_else(|| env::var("AAC_CODER").unwrap_or_else(|_| "fast".to_string())),
        threads_per_chunk: state.threads_per_chunk,
    };

    let render_started = Instant::now();
    let rendered = render_all(state, job, &source, &decision, &options, &output);
    let render_seconds = render_started.elapsed().as_secs_f64();
    let rendered = rendered?;

    if source.info.has_video {
        Calibration::observe(
            &state.calibration.pixel_rate,
            decision.kept_frames as f64 * source.info.width as f64 * source.info.height as f64,
            rendered.video_seconds,
            100_000.0,
            10_000_000_000.0,
        );
    }
    Calibration::observe(
        &state.calibration.audio_rate,
        kept_duration * source.info.audio.len().max(1) as f64,
        rendered.audio_seconds,
        1.0,
        10_000.0,
    );

    let output_bytes = std::fs::metadata(&output)
        .map(|meta| meta.len())
        .unwrap_or(0);

    let mut result = analysis_value;
    if let serde_json::Value::Object(ref mut map) = result {
        map.insert("output_path".into(), json!(output.to_string_lossy()));
        map.insert("output_bytes".into(), json!(output_bytes));
        map.insert("chunks".into(), json!(rendered.chunks));
        map.insert("rendered_frames".into(), json!(rendered.frames));
        map.insert("expected_frames".into(), json!(decision.kept_frames));
        map.insert("render_seconds".into(), json!(round2(render_seconds)));
        map.insert("video_seconds".into(), json!(round2(rendered.video_seconds)));
        map.insert("audio_seconds".into(), json!(round2(rendered.audio_seconds)));
        map.insert("total_seconds".into(), json!(round2(job.elapsed())));
        map.insert("workers".into(), json!(state.workers));
        map.insert("video_encoder".into(), json!(options.video_codec));
        map.insert("hardware_encoder".into(), json!(encoder_is_hardware));
        map.insert("video_pipeline".into(), json!(encoder_pipeline));
    }
    Ok(result)
}

struct Rendered {
    chunks: usize,
    frames: u64,
    /// When each part of the render finished, in seconds from the phase start.
    /// The two run concurrently, so these are what the estimate's `max(picture,
    /// audio)` model is calibrated against.
    video_seconds: f64,
    audio_seconds: f64,
}

/// Encode the picture in parallel chunks and the audio alongside it, then join
/// everything into `output`.
fn render_all(
    state: &AppState,
    job: &Job,
    source: &Source,
    decision: &Decision,
    options: &render::RenderOptions,
    output: &Path,
) -> WorkResult<Rendered> {
    let timebase = source.timebase.as_f64();
    let chunks = if source.info.has_video {
        render::plan_chunks(
            &decision.segments,
            timebase,
            state.workers,
            state.max_segments_per_chunk,
            state.min_chunk_seconds,
            state.max_gap_seconds,
        )
    } else {
        Vec::new()
    };

    let audio_total: u64 = decision
        .streams
        .iter()
        .map(|analysis| {
            let frames = analysis.levels.len();
            decision
                .segments
                .iter()
                .map(|&(start, end)| {
                    analysis.boundaries[end.min(frames)] - analysis.boundaries[start.min(frames)]
                })
                .sum::<u64>()
        })
        .sum();
    job.progress.set_audio_total(audio_total);

    let path = &source.path;
    let audio_extension = if options.audio_codec == "aac" {
        "m4a"
    } else {
        "mka"
    };

    let phase_started = Instant::now();
    let rendered = std::thread::scope(|scope| -> WorkResult<RenderedParts> {
        // Audio: one encoder per track, fed by our own sample-exact splice.
        let audio_handles: Vec<_> = source
            .info
            .audio
            .iter()
            .zip(decision.streams.iter())
            .map(|(stream, analysis)| {
                let track_path = state.work_dir.join(format!(
                    "{}-a{}.{audio_extension}",
                    job.id, stream.ordinal
                ));
                let frames = analysis.levels.len();
                let ranges: Vec<(u64, u64)> = decision
                    .segments
                    .iter()
                    .map(|&(start, end)| {
                        (
                            analysis.boundaries[start.min(frames)],
                            analysis.boundaries[end.min(frames)],
                        )
                    })
                    .filter(|&(start, end)| end > start)
                    .collect();
                scope.spawn(move || {
                    // No encode slot for audio: it is one thread, and it is the
                    // long pole on a long video (the picture is spread across
                    // every core, the audio encoder cannot be). Making it queue
                    // behind the chunks just moves the whole render's finish line.
                    audio::splice_to_encoder(
                        &state.ffmpeg,
                        path,
                        stream.ordinal,
                        stream.channels,
                        stream.sample_rate,
                        &ranges,
                        &track_path,
                        &options.audio_codec,
                        &options.audio_bitrate,
                        &options.audio_coder,
                        job,
                    )
                    .map(|_| (track_path, phase_started.elapsed().as_secs_f64()))
                })
            })
            .collect();

        // Picture: one encoder per chunk, each decoding only its own span.
        let video_handles: Vec<_> = chunks
            .iter()
            .enumerate()
            .map(|(index, chunk)| {
                scope.spawn(move || {
                    let _slot = state.slots.acquire();
                    render::render_chunk(
                        &state.ffmpeg,
                        path,
                        chunk,
                        index,
                        source.timebase,
                        options,
                        &state.work_dir,
                        &job.id,
                        job,
                    )
                    .map(|(path, frames)| (path, frames, phase_started.elapsed().as_secs_f64()))
                })
            })
            .collect();

        let mut failure: Option<WorkError> = None;
        let mut parts = RenderedParts::default();

        for handle in audio_handles {
            match handle.join() {
                Ok(Ok((path, finished))) => {
                    parts.audio_paths.push(path);
                    parts.audio_seconds = parts.audio_seconds.max(finished);
                }
                Ok(Err(err)) => failure = Some(failure.unwrap_or(err)),
                Err(_) => {
                    failure = Some(
                        failure
                            .unwrap_or(WorkError::Failed("audio encoding panicked".to_string())),
                    )
                }
            }
        }
        for handle in video_handles {
            match handle.join() {
                Ok(Ok((path, count, finished))) => {
                    parts.video_paths.push(path);
                    parts.frames += count;
                    parts.video_seconds = parts.video_seconds.max(finished);
                }
                Ok(Err(err)) => failure = Some(failure.unwrap_or(err)),
                Err(_) => {
                    failure = Some(
                        failure
                            .unwrap_or(WorkError::Failed("video encoding panicked".to_string())),
                    )
                }
            }
        }

        match failure {
            Some(err) => {
                for path in parts.audio_paths.iter().chain(parts.video_paths.iter()) {
                    let _ = std::fs::remove_file(path);
                }
                Err(err)
            }
            None => Ok(parts),
        }
    });

    let RenderedParts {
        video_paths,
        audio_paths,
        frames,
        video_seconds,
        audio_seconds,
    } = rendered?;

    // A lost frame here would mean silently dropping content, so the count ffmpeg
    // reported has to match the edit.
    if source.info.has_video && frames.abs_diff(decision.kept_frames as u64) > 2 {
        for path in audio_paths.iter().chain(video_paths.iter()) {
            let _ = std::fs::remove_file(path);
        }
        return Err(WorkError::Failed(format!(
            "rendered {frames} frames but the edit keeps {}; refusing to write a \
             file that does not match the edit",
            decision.kept_frames
        )));
    }

    job.progress.set_phase(Phase::Finishing);
    let result = render::concat_and_mux(
        &state.ffmpeg,
        &video_paths,
        &audio_paths,
        output,
        &state.work_dir,
        &job.id,
        job,
    );

    for path in audio_paths.iter().chain(video_paths.iter()) {
        let _ = std::fs::remove_file(path);
    }
    result?;

    Ok(Rendered {
        chunks: video_paths.len(),
        frames,
        video_seconds,
        audio_seconds,
    })
}

/// What the parallel part of the render produced.
#[derive(Default)]
struct RenderedParts {
    video_paths: Vec<PathBuf>,
    audio_paths: Vec<PathBuf>,
    frames: u64,
    video_seconds: f64,
    audio_seconds: f64,
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

fn canonicalize_existing(path: &str) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path))
}

fn resolve_existing_within(root: &Path, requested_path: &str) -> Result<PathBuf, String> {
    let candidate = PathBuf::from(requested_path);
    let candidate = if candidate.is_absolute() {
        candidate
    } else {
        root.join(candidate)
    };
    let canonical_candidate = std::fs::canonicalize(&candidate)
        .map_err(|_| format!("path does not exist: {}", candidate.display()))?;
    let canonical_root = std::fs::canonicalize(root)
        .map_err(|_| format!("invalid configured root: {}", root.display()))?;

    if !canonical_candidate.starts_with(&canonical_root) {
        return Err(format!(
            "path is outside allowed root: {}",
            candidate.display()
        ));
    }

    Ok(canonical_candidate)
}

fn resolve_output_within(root: &Path, requested_path: &str) -> Result<PathBuf, String> {
    let root = std::fs::canonicalize(root)
        .map_err(|_| format!("invalid configured root: {}", root.display()))?;
    let candidate = PathBuf::from(requested_path);

    if candidate
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err("output_path must not contain parent traversal".to_string());
    }

    let resolved = if candidate.is_absolute() {
        candidate
    } else {
        root.join(candidate)
    };

    if !resolved.starts_with(&root) {
        return Err(format!(
            "output_path is outside allowed root: {}",
            resolved.display()
        ));
    }

    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(duration: f64, width: u32, height: u32) -> MediaInfo {
        MediaInfo {
            has_video: true,
            frame_rate: Rational { num: 30, den: 1 },
            width,
            height,
            audio: vec![probe::AudioStream {
                ordinal: 0,
                sample_rate: 48000,
                channels: 2,
            }],
            duration,
        }
    }

    #[test]
    fn an_estimate_scales_with_the_work() {
        let calibration = Calibration {
            analyze_rate: Mutex::new(60.0),
            audio_rate: Mutex::new(80.0),
            pixel_rate: Mutex::new(40_000_000.0),
        };
        // Three hours of 1080p30, all of it kept.
        let long = calibration.estimate(&info(10800.0, 1920, 1080), 30.0, 10800.0, None, true, 4);
        assert!((long.analyze_seconds - 180.0).abs() < 1.0);
        // 324000 frames x 2Mpixel / 40Mpixel per second.
        assert!((long.render_seconds - 16796.16).abs() < 1.0);
        assert!(long.total_seconds > long.render_seconds);

        // Keeping half the footage halves the render.
        let half = calibration.estimate(&info(10800.0, 1920, 1080), 30.0, 5400.0, None, true, 4);
        assert!((half.render_seconds / long.render_seconds - 0.5).abs() < 1e-6);

        // An analyze-only job does not carry a render.
        let analyze_only =
            calibration.estimate(&info(600.0, 1920, 1080), 30.0, 600.0, None, false, 4);
        assert_eq!(analyze_only.render_seconds, 0.0);
        assert_eq!(analyze_only.finish_seconds, 0.0);
    }

    #[test]
    fn measurements_pull_the_rate_towards_what_the_machine_does() {
        let slot = Mutex::new(40_000_000.0);
        // Twice as fast as the seed, repeatedly.
        for _ in 0..20 {
            Calibration::observe(&slot, 80_000_000.0, 1.0, 1.0, 1e12);
        }
        let rate = *slot.lock().unwrap();
        assert!(rate > 79_000_000.0, "rate stuck at {rate}");
    }

    #[test]
    fn a_short_measurement_is_ignored() {
        let slot = Mutex::new(100.0);
        // Process startup dominates a sub-second run; it must not skew the rate.
        Calibration::observe(&slot, 1.0, 0.2, 1.0, 1e6);
        assert_eq!(*slot.lock().unwrap(), 100.0);
    }

    #[test]
    fn every_phase_carries_some_weight() {
        let calibration = Calibration {
            analyze_rate: Mutex::new(1e9),
            audio_rate: Mutex::new(1e9),
            pixel_rate: Mutex::new(1e12),
        };
        // Rates so high the estimate rounds to nothing; the analyze phase still
        // needs a non-zero weight or the bar would skip it.
        let weights = calibration
            .estimate(&info(60.0, 320, 240), 30.0, 60.0, None, true, 1)
            .weights();
        assert!(weights.analyze > 0.0);
    }
}
