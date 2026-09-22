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
mod sync;

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
    /// How many buckets the waveform is summarised into. 0 leaves it out.
    #[serde(default = "default_waveform_buckets")]
    waveform_buckets: usize,
}

fn default_waveform_buckets() -> usize {
    1200
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum RequestMode {
    Analyze,
    Trim,
    /// Measure how far apart two recordings started, and stop there.
    Sync,
}

/// Which soundtrack decides the edit when there are two files.
#[derive(Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
enum EditSource {
    /// A frame survives if either recording was loud there. Two files stacked on
    /// one timeline are two tracks of one edit, and `--edit audio` keeps a frame
    /// any one of its tracks speaks in — so the camera does not cut away from a
    /// moment only the screen recording heard.
    #[default]
    Both,
    /// Only the first recording is listened to.
    Primary,
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
    #[serde(default)]
    audio_fade_ms: Option<f64>,

    // --- the second recording, for a synced pair -----------------------------
    /// A camera take and a screen capture of the same session. With this set,
    /// `sync` measures the gap between them and `trim` cuts both to one timeline.
    #[serde(default)]
    second_video_path: Option<String>,
    /// Required to trim a pair.
    #[serde(default)]
    second_output_path: Option<String>,
    /// A gap already measured, or corrected by hand, in seconds: where the second
    /// recording starts on the first one's clock. Skips the search.
    #[serde(default)]
    offset_seconds: Option<f64>,
    /// Whether to check that the two recorders kept the same time. On by default;
    /// it costs a few excerpt reads and catches the slip that makes a long pair
    /// drift apart by the end.
    #[serde(default)]
    check_drift: Option<bool>,
    #[serde(default)]
    edit_source: EditSource,
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

/// The loudness envelope, before and after the cut, summarised to something a
/// browser can draw. Peak per bucket rather than an average: an average smears
/// the quiet gaps into the speech around them, which is exactly the distinction
/// the picture is meant to show.
#[derive(Serialize)]
struct Waveform {
    buckets: usize,
    /// Over the whole timeline.
    before: Vec<u8>,
    /// Over the kept frames only, so it lines up with the trimmed file.
    after: Vec<u8>,
    /// How much of each bucket survives the edit, 0 to 255. Bucket resolution is
    /// all a drawing needs, and unlike the segment list it stays the same 1.2 kB
    /// whether the edit has four cuts or four thousand — so the picture still
    /// draws after a reload, from what the render stored.
    kept: Vec<u8>,
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
    /// Source shape, carried through so an edit can be exported to an editor
    /// (FCPXML needs the picture size and the audio layout).
    has_video: bool,
    width: u32,
    height: u32,
    sample_rate: u32,
    channels: usize,
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
    waveform: Option<Waveform>,
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
        .route("/jobs/:job_id/segments", get(job_segments))
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

/// Every kept range of a finished job, uncapped.
///
/// The status response caps its segment list, because a poll every second should
/// not carry a megabyte of ranges. An export needs all of them, and asks once.
async fn job_segments(
    State(state): State<Arc<AppState>>,
    UrlPath(job_id): UrlPath<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let job = state
        .jobs
        .get(&job_id)
        .ok_or((StatusCode::NOT_FOUND, "no such job".to_string()))?;
    let segments = job
        .segments
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
        .ok_or((
            StatusCode::CONFLICT,
            "this job has not finished its analysis".to_string(),
        ))?;
    Ok(Json(segments))
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
    if request.mode == RequestMode::Sync && request.second_video_path.is_none() {
        return Err(bad_request(
            "second_video_path is required to line two recordings up",
        ));
    }
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
    let second = match request.second_video_path.as_deref().filter(|p| !p.is_empty()) {
        Some(path) => Some(open_source(&state, path)?),
        None => None,
    };
    let second_output = match (trim, &second) {
        (true, Some(_)) => {
            let requested = request
                .second_output_path
                .as_deref()
                .filter(|path| !path.is_empty())
                .ok_or_else(|| {
                    bad_request("second_output_path is required to trim a pair")
                })?;
            Some(resolve_output_within(&state.video_root, requested).map_err(bad_request)?)
        }
        _ => None,
    };
    let estimate = state.calibration.estimate(
        &source.info,
        source.timebase.as_f64(),
        source.info.duration,
        Some(1.0),
        trim,
        state.workers,
    );

    let mode = match request.mode {
        RequestMode::Trim => Mode::Trim,
        RequestMode::Sync => Mode::Sync,
        RequestMode::Analyze => Mode::Analyze,
    };
    let job = state
        .jobs
        .create(mode, source.path.to_string_lossy().into_owned())
        .map_err(|active| Problem {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: format!(
                "{active} trim jobs are already running; try again when one finishes"
            ),
        })?;

    // A sync job only decodes; weighting it with a render and a mux it will never
    // do would leave the bar crawling through a first phase that is the whole job.
    job.progress.set_weights(if request.mode == RequestMode::Sync {
        PhaseWeights {
            analyze: 1.0,
            render: 0.0,
            finish: 0.0,
        }
    } else {
        estimate.weights()
    });
    job.progress.set_phase(Phase::Analyzing);
    if request.mode == RequestMode::Sync {
        let second_duration = second.as_ref().map(|s| s.info.duration).unwrap_or(0.0);
        job.progress
            .set_expected_frames(sync_analysis_units(source.info.duration, second_duration));
    } else {
        let frames = |source: &Source| {
            (source.info.duration * source.timebase.as_f64()).round() as u64
                * source.info.audio.len().max(1) as u64
        };
        job.progress.set_expected_frames(
            frames(&source) + second.as_ref().map(frames).unwrap_or(0),
        );
    }
    if request.mode != RequestMode::Sync {
        // The estimate models a trim, so it would only mislead a job that renders
        // nothing; the measured pace fills the ETA in soon enough on its own.
        if let Ok(mut slot) = job.estimate.lock() {
            *slot = serde_json::to_value(&estimate).ok();
        }
    }

    let status = status_of(&job);
    let worker_state = Arc::clone(&state);
    let worker_job = Arc::clone(&job);
    std::thread::spawn(move || {
        let outcome = run_job(
            &worker_state,
            &worker_job,
            &request,
            source,
            second,
            output,
            second_output,
        );
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

/// The loudest level in each bucket, as a byte. Peaks are what a waveform is
/// made of; averaging would hide the quiet frames the edit turns on.
fn peaks(levels: &[u16], buckets: usize) -> Vec<u8> {
    if levels.is_empty() || buckets == 0 {
        return Vec::new();
    }
    let buckets = buckets.min(levels.len());
    let mut out = Vec::with_capacity(buckets);
    for bucket in 0..buckets {
        let start = bucket * levels.len() / buckets;
        let end = (((bucket + 1) * levels.len() / buckets).max(start + 1)).min(levels.len());
        let peak = levels[start..end].iter().copied().max().unwrap_or(0);
        out.push((peak >> 8) as u8);
    }
    out
}

/// The share of each bucket the edit keeps, on the same buckets as `peaks`.
fn kept_share(mask: &[bool], buckets: usize) -> Vec<u8> {
    if mask.is_empty() || buckets == 0 {
        return Vec::new();
    }
    let buckets = buckets.min(mask.len());
    let mut out = Vec::with_capacity(buckets);
    for bucket in 0..buckets {
        let start = bucket * mask.len() / buckets;
        let end = (((bucket + 1) * mask.len() / buckets).max(start + 1)).min(mask.len());
        let kept = mask[start..end].iter().filter(|&&keep| keep).count();
        out.push((kept * 255 / (end - start)) as u8);
    }
    out
}

/// One envelope for the whole file, loudest stream winning per frame — the same
/// "any stream is loud enough" rule the edit itself uses.
fn combined_levels(streams: &[audio::StreamAnalysis], total_frames: usize) -> Vec<u16> {
    let mut levels = vec![0u16; total_frames];
    for stream in streams {
        for (index, &level) in stream.levels.iter().enumerate() {
            if level > levels[index] {
                levels[index] = level;
            }
        }
    }
    levels
}

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
    /// The same edit as the second recording sees it, when there is one.
    pair: Option<PairEdit>,
}

/// The second recording's half of a shared edit: the same moments, counted in its
/// own frames, and how its clock relates to the first one's.
struct PairEdit {
    segments: Vec<(usize, usize)>,
    streams: Vec<audio::StreamAnalysis>,
    clock: sync::Clock,
}

impl PairEdit {
    fn kept_frames(&self) -> usize {
        self.segments.iter().map(|&(start, end)| end - start).sum()
    }

    fn cut(&self) -> Cut<'_> {
        Cut {
            streams: &self.streams,
            segments: &self.segments,
            kept_frames: self.kept_frames(),
        }
    }
}

/// What a render needs: which moments to keep, and where each one begins and ends
/// in each soundtrack. Borrowed rather than owned, because for a three-hour file
/// this is megabytes of levels that the render only reads.
struct Cut<'a> {
    streams: &'a [audio::StreamAnalysis],
    segments: &'a [(usize, usize)],
    kept_frames: usize,
}

impl Decision {
    fn cut(&self) -> Cut<'_> {
        Cut {
            streams: &self.streams,
            segments: &self.segments,
            kept_frames: self.kept_frames,
        }
    }
}

fn analyze(
    state: &AppState,
    job: &Job,
    request: &EditRequest,
    source: &Source,
) -> WorkResult<Decision> {
    let started = Instant::now();
    let streams = stream_levels(state, job, source)?;
    decide(state, request, source, streams, None, None, started)
}

/// How much decoding a sync job has ahead of it, in the units the envelope reader
/// counts in — one per 512 bytes of 16-bit mono audio.
fn sync_analysis_units(a_duration: f64, b_duration: f64) -> u64 {
    let units = |seconds: f64| seconds * sync::DECODE_RATE as f64 * 2.0 / 512.0;
    // Both files end to end, plus the excerpts the refinement and the drift check
    // read back: six windows, both sides, a little over the window length each.
    units(a_duration + b_duration) as u64 + (units(WINDOW_READS) * 12.0) as u64
}

/// Seconds of audio one refinement window reads from one file.
const WINDOW_READS: f64 = 34.0;

/// Every audio track of one file, as levels per frame.
///
/// One decoder per track, in parallel: `--edit audio` analyzes every stream, and a
/// frame survives if any one of them is loud enough.
fn stream_levels(
    state: &AppState,
    job: &Job,
    source: &Source,
) -> WorkResult<Vec<audio::StreamAnalysis>> {
    let timebase = source.timebase.as_f64();
    let path = &source.path;

    std::thread::scope(|scope| {
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
    })
}

/// Turn levels into an edit: threshold, margins, smoothing, and what it all adds
/// up to.
///
/// `extra_levels` is a second recording's loudness laid over this one's frames, so
/// a pair can be cut on what either of them heard. `limit` narrows the edit to a
/// range of frames, which is how a pair is held to the stretch both files cover.
#[allow(clippy::too_many_arguments)]
fn decide(
    state: &AppState,
    request: &EditRequest,
    source: &Source,
    streams: Vec<audio::StreamAnalysis>,
    extra_levels: Option<&[u16]>,
    limit: Option<(usize, usize)>,
    started: Instant,
) -> WorkResult<Decision> {
    let timebase = source.timebase.as_f64();
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
    if let Some(levels) = extra_levels {
        for (index, &level) in levels.iter().take(total_frames).enumerate() {
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
    if let Some((first, last)) = limit {
        // After the margins, so a margin cannot reach past the end of what the
        // other recording covers, and on the mask rather than the segments so the
        // waveform the browser draws shows the same edit that was rendered.
        for frame in 0..total_frames {
            if frame < first || frame >= last {
                active[frame] = false;
            }
        }
    }

    let segments = mask::active_segments(&active);
    let kept_frames: usize = segments.iter().map(|&(start, end)| end - start).sum();
    let removed_frames = total_frames - kept_frames;

    let limit = request.max_segments_returned.unwrap_or(usize::MAX);

    let waveform = if request.waveform_buckets > 0 {
        let levels = combined_levels(&streams, total_frames);
        let kept: Vec<u16> = segments
            .iter()
            .flat_map(|&(start, end)| levels[start..end].iter().copied())
            .collect();
        let before = peaks(&levels, request.waveform_buckets);
        Some(Waveform {
            buckets: before.len(),
            kept: kept_share(&active, request.waveform_buckets),
            before,
            after: peaks(&kept, request.waveform_buckets),
        })
    } else {
        None
    };

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
        has_video: source.info.has_video,
        width: source.info.width,
        height: source.info.height,
        sample_rate: source.info.audio.first().map(|a| a.sample_rate).unwrap_or(0),
        channels: source.info.audio.first().map(|a| a.channels).unwrap_or(0),
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
        waveform,
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
        pair: None,
    })
}

/// One track of a file, named the way the second recording is asked for.
fn first_track(source: &Source) -> sync::Track {
    sync::Track {
        path: source.path.clone(),
        ordinal: source.info.audio.first().map(|a| a.ordinal).unwrap_or(0),
    }
}

/// Measure the gap between two recordings, or take the one the caller supplied.
fn line_up(
    state: &AppState,
    job: &Job,
    request: &JobRequest,
    source: &Source,
    second: &Source,
) -> WorkResult<sync::SyncReport> {
    if let Some(offset) = request.offset_seconds {
        if !offset.is_finite() {
            return Err(WorkError::Failed("offset_seconds must be a number".to_string()));
        }
        // Given by hand, so it is taken as read; someone who typed it in has
        // already decided they trust it more than a measurement.
        return Ok(sync::SyncReport::supplied(
            offset,
            source.info.duration,
            second.info.duration,
        ));
    }
    sync::measure(
        &state.ffmpeg,
        &first_track(source),
        &first_track(second),
        source.info.duration,
        second.info.duration,
        request.check_drift.unwrap_or(true),
        job,
    )
}

/// Decide one edit for two recordings.
///
/// The edit lives on the first recording's clock and is held to the stretch both
/// files cover, so the two cuts are the same moments and the same length. Nothing
/// outside the overlap can be kept: one file has no picture there, and a pair
/// where one side is missing frames is a pair that never lines up again.
fn analyze_pair(
    state: &AppState,
    job: &Job,
    request: &JobRequest,
    source: &Source,
    second: &Source,
    report: &sync::SyncReport,
) -> WorkResult<Decision> {
    if !report.reliable {
        return Err(WorkError::Failed(
            report
                .warning
                .clone()
                .unwrap_or_else(|| "these two recordings could not be lined up".to_string()),
        ));
    }
    let started = Instant::now();
    let timebase = source.timebase.as_f64();
    let clock = report.clock();

    let streams = stream_levels(state, job, source)?;
    // The second recording is decoded whichever soundtrack decides the edit: its
    // sample boundaries are what the splice cuts on.
    let second_streams = stream_levels(state, job, second)?;

    let total_frames = streams
        .iter()
        .map(|stream| stream.levels.len())
        .max()
        .unwrap_or(0);
    let extra = match request.edit_source {
        EditSource::Both => {
            let second_frames = second_streams
                .iter()
                .map(|stream| stream.levels.len())
                .max()
                .unwrap_or(0);
            Some(sync::map_levels(
                &combined_levels(&second_streams, second_frames),
                second.timebase.as_f64(),
                &clock,
                total_frames,
                timebase,
            ))
        }
        EditSource::Primary => None,
    };

    let overlap = sync::shared_frames(report.overlap_on_a(), timebase);
    let mut decision = decide(
        state,
        &request.edit,
        source,
        streams,
        extra.as_deref(),
        Some(overlap),
        started,
    )?;
    decision.pair = Some(PairEdit {
        segments: sync::shift_segments(
            &decision.segments,
            timebase,
            second.timebase.as_f64(),
            &clock,
            second_streams
                .iter()
                .map(|stream| stream.levels.len())
                .max()
                .unwrap_or(0),
        ),
        streams: second_streams,
        clock,
    });
    Ok(decision)
}

#[allow(clippy::too_many_arguments)]
fn run_job(
    state: &AppState,
    job: &Job,
    request: &JobRequest,
    source: Source,
    second: Option<Source>,
    output: Option<PathBuf>,
    second_output: Option<PathBuf>,
) -> WorkResult<serde_json::Value> {
    // A pair has to be put on one clock before anything can be decided about it.
    let pair = match &second {
        Some(second) => Some(line_up(state, job, request, &source, second)?),
        None => None,
    };

    if request.mode == RequestMode::Sync {
        let report = pair.expect("a sync job is refused without a second file");
        job.progress.set_phase(Phase::Finished);
        return serde_json::to_value(json!({
            "sync": report,
            "video_path": source.path.to_string_lossy(),
            "second_video_path": second.map(|s| s.path.to_string_lossy().into_owned()),
        }))
        .map_err(|err| WorkError::Failed(err.to_string()));
    }

    let decision = match (&second, &pair) {
        (Some(second), Some(report)) => analyze_pair(state, job, request, &source, second, report)?,
        _ => analyze(state, job, &request.edit, &source)?,
    };
    let analysis_value = serde_json::to_value(&decision.analysis)
        .map_err(|err| WorkError::Failed(err.to_string()))?;
    if let Ok(mut slot) = job.analysis.lock() {
        *slot = Some(analysis_value.clone());
    }
    if let Ok(mut slot) = job.segments.lock() {
        let timebase = source.timebase.as_f64();
        let second_ranges = decision.pair.as_ref().zip(second.as_ref()).map(|(pair, file)| {
            let second_timebase = file.timebase.as_f64();
            json!({
                "video_path": file.path.to_string_lossy(),
                "clock": pair.clock,
                "timebase": {"num": file.timebase.num, "den": file.timebase.den},
                "fps": second_timebase,
                "segment_count": pair.segments.len(),
                "segments": pair
                    .segments
                    .iter()
                    .map(|&(start, end)| json!({
                        "start_frame": start,
                        "end_frame": end,
                        "start": start as f64 / second_timebase,
                        "end": end as f64 / second_timebase,
                    }))
                    .collect::<Vec<_>>(),
            })
        });
        *slot = Some(json!({
            "timebase": {"num": source.timebase.num, "den": source.timebase.den},
            "fps": timebase,
            "segment_count": decision.segments.len(),
            "segments": decision
                .segments
                .iter()
                .map(|&(start, end)| json!({
                    "start_frame": start,
                    "end_frame": end,
                    "start": start as f64 / timebase,
                    "end": end as f64 / timebase,
                }))
                .collect::<Vec<_>>(),
            "sync": pair,
            "second": second_ranges,
        }));
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
        // auto-editor ramps 3ms at every clip edge to keep splices from clicking.
        audio_fade_ms: request.audio_fade_ms.unwrap_or_else(|| env_f64("AUDIO_FADE_MS", 3.0)),
        threads_per_chunk: state.threads_per_chunk,
    };

    // A pair renders twice, so the bar has to count both from the start;
    // otherwise it reaches the end halfway through the job.
    if let Some(pair) = &decision.pair {
        job.progress
            .set_kept_frames((decision.kept_frames + pair.kept_frames()) as u64);
    }

    let render_started = Instant::now();
    let rendered = render_all(state, job, &source, &decision.cut(), &options, &output, &job.id);
    let render_seconds = render_started.elapsed().as_secs_f64();
    let rendered = rendered?;

    // The second recording, cut to the same moments. In sequence rather than
    // alongside: each render already spreads itself across every core, and two at
    // once would only take turns.
    let second_rendered = match (&decision.pair, &second, &second_output) {
        (Some(pair), Some(file), Some(path)) => {
            let rendered = render_all(
                state,
                job,
                file,
                &pair.cut(),
                &options,
                path,
                &format!("{}-second", job.id),
            )?;
            Some((path, pair, file, rendered))
        }
        _ => None,
    };

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
        if let Some(report) = &pair {
            map.insert("sync".into(), json!(report));
        }
        if let Some((path, pair, file, rendered)) = &second_rendered {
            let bytes = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
            let fps = file.timebase.as_f64();
            let kept_frames = pair.kept_frames();
            map.insert(
                "second".into(),
                json!({
                    "video_path": file.path.to_string_lossy(),
                    "output_path": path.to_string_lossy(),
                    "output_bytes": bytes,
                    "segment_count": pair.segments.len(),
                    "kept_frames": kept_frames,
                    "rendered_frames": rendered.frames,
                    "fps": fps,
                    "output_duration": kept_frames as f64 / fps,
                    "chunks": rendered.chunks,
                }),
            );
        }
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
/// `tag` names this render's scratch files. A pair renders twice inside one job,
/// and two renders sharing a name would overwrite each other's chunks.
#[allow(clippy::too_many_arguments)]
fn render_all(
    state: &AppState,
    job: &Job,
    source: &Source,
    cut: &Cut,
    options: &render::RenderOptions,
    output: &Path,
    tag: &str,
) -> WorkResult<Rendered> {
    let timebase = source.timebase.as_f64();
    let chunks = if source.info.has_video {
        render::plan_chunks(
            cut.segments,
            timebase,
            state.workers,
            state.max_segments_per_chunk,
            state.min_chunk_seconds,
            state.max_gap_seconds,
        )
    } else {
        Vec::new()
    };

    let audio_total: u64 = cut
        .streams
        .iter()
        .map(|analysis| {
            let frames = analysis.levels.len();
            cut.segments
                .iter()
                .map(|&(start, end)| {
                    analysis.boundaries[end.min(frames)] - analysis.boundaries[start.min(frames)]
                })
                .sum::<u64>()
        })
        .sum();
    job.progress.add_audio_total(audio_total);

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
            .zip(cut.streams.iter())
            .map(|(stream, analysis)| {
                let track_path = state.work_dir.join(format!(
                    "{tag}-a{}.{audio_extension}",
                    stream.ordinal
                ));
                let frames = analysis.levels.len();
                let ranges: Vec<(u64, u64)> = cut
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
                        options.audio_fade_ms,
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
                        tag,
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
    if source.info.has_video && frames.abs_diff(cut.kept_frames as u64) > 2 {
        for path in audio_paths.iter().chain(video_paths.iter()) {
            let _ = std::fs::remove_file(path);
        }
        return Err(WorkError::Failed(format!(
            "rendered {frames} frames but the edit keeps {}; refusing to write a \
             file that does not match the edit",
            cut.kept_frames
        )));
    }

    job.progress.set_phase(Phase::Finishing);
    let result = render::concat_and_mux(
        &state.ffmpeg,
        &video_paths,
        &audio_paths,
        output,
        &state.work_dir,
        tag,
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
    fn a_waveform_keeps_the_peak_of_each_bucket() {
        // Averaging would smear a quiet gap into the speech beside it; the peak is
        // what shows where the edit will cut.
        let levels: Vec<u16> = vec![0, 0, 60000, 0, 100, 200, 0, 0];
        let drawn = peaks(&levels, 4);
        assert_eq!(drawn, vec![0, 234, 0, 0]);
        assert_eq!(drawn.len(), 4);
    }

    #[test]
    fn a_waveform_never_invents_buckets() {
        // Fewer frames than buckets: one bucket per frame, not padding.
        assert_eq!(peaks(&[65535, 0, 65535], 100).len(), 3);
        assert!(peaks(&[], 100).is_empty());
        assert!(peaks(&[1, 2, 3], 0).is_empty());
    }

    #[test]
    fn the_kept_share_says_how_much_of_each_bucket_survives() {
        // Four frames per bucket: all kept, none kept, half kept.
        let mask = [true, true, true, true, false, false, false, false, true, true, false, false];
        assert_eq!(kept_share(&mask, 3), vec![255, 0, 127]);
        assert!(kept_share(&[], 10).is_empty());
        assert!(kept_share(&[true], 0).is_empty());
    }

    #[test]
    fn the_envelope_takes_the_loudest_stream_per_frame() {
        // The edit keeps a frame if *any* stream is loud enough, so the drawing
        // has to agree with it.
        let quiet = audio::StreamAnalysis {
            levels: vec![10, 20, 30],
            boundaries: vec![0, 1, 2, 3],
        };
        let loud = audio::StreamAnalysis {
            levels: vec![5, 900, 0],
            boundaries: vec![0, 1, 2, 3],
        };
        assert_eq!(combined_levels(&[quiet, loud], 3), vec![10, 900, 30]);
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
