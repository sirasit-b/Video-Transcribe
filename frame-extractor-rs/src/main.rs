use axum::{extract::State, http::StatusCode, routing::{get, post}, Json, Router};
use serde::{Deserialize, Serialize};
use std::{env, net::SocketAddr, path::{Component, Path, PathBuf}, sync::Arc};
use tokio::sync::Semaphore;

#[derive(Clone)]
struct AppState {
    video_root: PathBuf,
    output_root: PathBuf,
}

#[derive(Deserialize)]
struct ExtractFramesRequest {
    video_path: String,
    frame_count: u32,
    output_dir: String,
}

#[derive(Serialize)]
struct ExtractFramesResponse {
    frame_paths: Vec<String>,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[tokio::main]
async fn main() {
    let host = env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
    let port = env::var("PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(8081);

    let state = Arc::new(AppState {
        video_root: canonicalize_existing(&env::var("VIDEO_ROOT").unwrap_or_else(|_| "/app/videos".to_string())),
        output_root: canonicalize_existing(&env::var("OUTPUT_ROOT").unwrap_or_else(|_| "/app/frames".to_string())),
    });

    let app = Router::new()
        .route("/health", get(health))
        .route("/extract-frames", post(extract_frames))
        .with_state(state);

    let addr: SocketAddr = format!("{host}:{port}").parse().expect("invalid bind address");
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind tcp listener");

    axum::serve(listener, app).await.expect("server failed");
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

async fn extract_frames(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<ExtractFramesRequest>,
) -> Result<Json<ExtractFramesResponse>, (StatusCode, String)> {
    if payload.frame_count == 0 {
        return Err((StatusCode::BAD_REQUEST, "frame_count must be at least 1".to_string()));
    }

    let video_path = resolve_existing_within(&state.video_root, &payload.video_path)
        .map_err(|message| (StatusCode::BAD_REQUEST, message))?;
    let output_dir = resolve_output_within(&state.output_root, &payload.output_dir)
        .map_err(|message| (StatusCode::BAD_REQUEST, message))?;

    std::fs::create_dir_all(&output_dir)
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;

    let duration = get_video_duration_seconds(&video_path)
        .await
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err))?;

    let mut tasks = tokio::task::JoinSet::new();
    // Bound concurrent ffmpeg jobs to the CPU count so we don't thrash the scheduler.
    let permits = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(4);
    let semaphore = Arc::new(Semaphore::new(permits));
    for index in 0..payload.frame_count {
        let timestamp = duration * (index as f64 + 0.5) / payload.frame_count as f64;
        let frame_path = output_dir.join(format!("frame_{:03}.png", index + 1));
        let video_path_clone = video_path.clone();
        let semaphore = Arc::clone(&semaphore);

        tasks.spawn(async move {
            let _permit = semaphore.acquire_owned().await.expect("semaphore closed");
            run_ffmpeg_extract(&video_path_clone, timestamp, &frame_path)
                .await
                .map(|_| (index, frame_path))
        });
    }

    let mut results: Vec<(u32, String)> = Vec::with_capacity(payload.frame_count as usize);
    while let Some(join_result) = tasks.join_next().await {
        match join_result {
            Ok(Ok((index, path))) => results.push((index, path.to_string_lossy().into_owned())),
            Ok(Err(err)) => return Err((StatusCode::INTERNAL_SERVER_ERROR, err)),
            Err(err) => return Err((StatusCode::INTERNAL_SERVER_ERROR, err.to_string())),
        }
    }

    results.sort_by_key(|(index, _)| *index);
    let frame_paths = results.into_iter().map(|(_, path)| path).collect();

    Ok(Json(ExtractFramesResponse { frame_paths }))
}

fn canonicalize_existing(path: &str) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path))
}

fn resolve_existing_within(root: &Path, requested_path: &str) -> Result<PathBuf, String> {
    let candidate = PathBuf::from(requested_path);
    let canonical_candidate = std::fs::canonicalize(&candidate)
        .map_err(|_| format!("path does not exist: {}", candidate.display()))?;
    let canonical_root = std::fs::canonicalize(root)
        .map_err(|_| format!("invalid configured root: {}", root.display()))?;

    if !canonical_candidate.starts_with(&canonical_root) {
        return Err(format!("path is outside allowed root: {}", candidate.display()));
    }

    Ok(canonical_candidate)
}

fn resolve_output_within(root: &Path, requested_path: &str) -> Result<PathBuf, String> {
    let root = std::fs::canonicalize(root)
        .map_err(|_| format!("invalid configured root: {}", root.display()))?;
    let candidate = PathBuf::from(requested_path);

    if candidate.components().any(|component| matches!(component, Component::ParentDir)) {
        return Err("output_dir must not contain parent traversal".to_string());
    }

    let resolved = if candidate.is_absolute() {
        candidate
    } else {
        root.join(candidate)
    };

    if !resolved.starts_with(&root) {
        return Err(format!("output_dir is outside allowed root: {}", resolved.display()));
    }

    Ok(resolved)
}

fn get_video_duration_seconds(video_path: &Path) -> impl std::future::Future<Output = Result<f64, String>> + Send {
    let video_path = video_path.to_owned();
    async move {
        let output = tokio::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "default=noprint_wrappers=1:nokey=1",
            ])
            .arg(&video_path)
            .output()
            .await
            .map_err(|err| format!("failed to run ffprobe: {err}"))?;

        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
        }

        let value = String::from_utf8_lossy(&output.stdout);
        let duration = value
            .trim()
            .parse::<f64>()
            .map_err(|err| format!("failed to parse duration: {err}"))?;

        if duration <= 0.0 {
            return Err("video duration must be greater than zero".to_string());
        }

        Ok(duration)
    }
}

fn run_ffmpeg_extract(video_path: &Path, timestamp: f64, frame_path: &Path) -> impl std::future::Future<Output = Result<(), String>> + Send {
    let video_path = video_path.to_owned();
    let frame_path = frame_path.to_owned();
    async move {
        let output = tokio::process::Command::new("ffmpeg")
            .args([
                "-noaccurate_seek",
                "-ss",
                &format!("{timestamp:.3}"),
                "-i",
            ])
            .arg(&video_path)
            .args([
                "-an",
                "-frames:v",
                "1",
                "-q:v",
                "2",
                "-threads",
                "1",
                "-y",
            ])
            .arg(&frame_path)
            .output()
            .await
            .map_err(|err| format!("failed to run ffmpeg: {err}"))?;

        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
        }

        Ok(())
    }
}