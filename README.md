# Video ASR Pipeline

Full-stack video service for uploading videos, generating Thai transcripts with timestamped captions, and extracting frames. Any department can use it directly — there is no course/project grouping layer.

## Tech Stack

- Frontend: Next.js 16 + React 19
- Backend: FastAPI + SQLAlchemy
- Database: PostgreSQL 15
- Media processing: FFmpeg, with frame extraction offloaded to a Rust service in Docker
- AI: Google Gemini (`gemini-3.5-flash`, timestamped Thai transcription) or OpenAI (`gpt-transcribe`, `whisper-1`, ...), picked per run; OpenAI `gpt-5` for caption proofreading

## Project Structure

```text
.
|-- api.py                  # FastAPI application and endpoints
|-- database.py             # SQLAlchemy engine/session setup
|-- models.py               # Database models: Video
|-- pipeline.py             # Audio extraction, transcription, SRT/VTT captions, frame extraction adapter
|-- docker-compose.yml      # Multi-service local stack
|-- Dockerfile              # Backend image
|-- frame-extractor-rs/     # Rust microservice for frame extraction
|-- frontend/               # Next.js app
|-- videos/                 # Uploaded video files
`-- texts/                  # Generated transcript and .srt files
```

## Features

- Upload videos directly, no grouping required
- Stream uploaded videos
- Extract evenly spaced frames from videos
- Transcribe video audio to Thai text with per-segment timestamps (Gemini), choosing the model per run
- Preview synced captions alongside video playback (click-to-seek cue list + native subtitle track)
- Edit any caption line by hand to fix ASR typos

## Prerequisites

- Docker Desktop (recommended)
- Or local tools:
  - Python 3.11+
  - Node.js 22+
  - PostgreSQL 15+
  - FFmpeg

## Environment Variables

Create `.env` in the project root:

```env
# Transcription
GEMINI_API_KEY=your_gemini_api_key
# Optional override (default: gemini-3.5-flash). The model must support both audio
# input and JSON mode; the gemini-*-transcribe family does not. gemini-3.1-pro-preview
# works only on a billing-enabled project.
# GEMINI_TRANSCRIBE_MODEL=gemini-3.1-pro-preview
# Extra ids for the UI model picker, comma-separated.
# GEMINI_TRANSCRIBE_MODELS=gemini-2.5-flash,gemini-2.5-pro
# OPENAI_TRANSCRIBE_MODELS=gpt-4o-transcribe-diarize
# Default model for the picker, across providers (OpenAI ids allowed).
# TRANSCRIBE_MODEL=gpt-transcribe

# Only needed for POST /api/videos/{id}/rewrite-captions
OPENAI_API_KEY=your_openai_api_key

# Optional when running backend outside docker:
# DATABASE_URL=postgresql://user:password@localhost:6879/borntodev_db
```

## Run with Docker (Recommended)

1. Build and start all services:

```bash
docker compose up --build
```

2. Open apps:

- Frontend: http://localhost:4287
- Backend API docs: http://localhost:8734/docs
- PostgreSQL host port: `6879`

The backend keeps the same frame extraction API, but delegates the heavy frame extraction step to the internal Rust service over HTTP.

3. Stop services:

```bash
docker compose down
```

4. Stop and remove DB volume (reset data):

```bash
docker compose down -v
```

## Run Locally Without Docker

### 1) Database

Start PostgreSQL and create a database named `borntodev_db`.

### 2) Backend

```bash
python -m venv .venv
# Windows PowerShell
.venv\Scripts\Activate.ps1
pip install -r requirements.txt
uvicorn api:app --reload --host 0.0.0.0 --port 8734
```

### 3) Frontend

```bash
cd frontend
npm install
npm run dev
```

## API Overview

Base URL: `http://localhost:8734`

- `POST /api/videos` - upload video file
- `GET /api/videos` - list all videos
- `GET /api/videos/{video_id}` - get video
- `GET /api/videos/stream/{filename}` - stream video
- `POST /api/videos/{video_id}/frames` - extract frames for a video
- `GET /api/videos/{video_id}/frames` - list extracted frames
- `GET /api/videos/{video_id}/frames/{frame_filename}` - fetch an extracted frame
- `GET /api/transcribe-models` - transcription models the UI can offer
- `POST /api/videos/{video_id}/transcribe` - generate transcript + caption segments (optional body: `{"model": "<id>"}`)
- `GET /api/videos/{video_id}/captions.vtt` - WebVTT captions (for the video `<track>` element)
- `GET /api/videos/{video_id}/captions.srt` - SRT captions (download)
- `PATCH /api/videos/{video_id}/captions/{segment_index}` - edit one caption line (`{"text": "..."}`)

Interactive docs:

- Swagger UI: `/docs`
- ReDoc: `/redoc`

## Transcription Models

The model is chosen per run from the picker on the video page; `GET /api/transcribe-models`
lists what the configured API keys allow.

| Model | Provider | Timestamps | Notes |
|---|---|---|---|
| `gemini-3.5-flash` | Gemini | native | Default. Works on a free-tier key. |
| `gemini-3.1-pro-preview` | Gemini | native | Needs a billing-enabled project. |
| `gpt-transcribe` | OpenAI | derived | Most accurate on Thai in our testing. |
| `gpt-4o-transcribe` | OpenAI | derived | Previous generation. |
| `gpt-4o-mini-transcribe` | OpenAI | derived | Cheapest. |
| `whisper-1` | OpenAI | native | Weakest text; returns its own segment timings. |

The `gpt-*transcribe` models reject `response_format=verbose_json` and return text with
no timings at all. For those, `pipeline._silence_cue_bounds()` cuts the audio into
2-12s windows in the middle of pauses found by ffmpeg `silencedetect`, transcribes each
window separately, and takes the cue timing from the cut points.

## Typical Workflow

1. Upload a video from the homepage.
2. Open the video page, pick a transcription model, and trigger transcription.
3. Preview the synced caption list, or toggle native subtitles on the player.
4. Click the pencil (or double-click a cue) to hand-fix any mis-transcribed line.
5. Extract frames as needed.

## Troubleshooting

- Frontend container exits with `exec format error`:
  - Rebuild images with no cache:
    ```bash
    docker compose build --no-cache
    docker compose up
    ```

- Backend cannot connect to DB:
  - Confirm DB container is healthy:
    ```bash
    docker compose ps
    ```
  - Check backend logs:
    ```bash
    docker compose logs backend
    ```

- Transcription key error (`GEMINI_API_KEY is not set`):
  - Ensure `.env` exists in root and contains a valid Gemini API key.

- Transcription fails with `429 RESOURCE_EXHAUSTED ... limit: 0`:
  - The configured model is not available on the Gemini free tier (this is the case for
    `gemini-3.1-pro-preview`). Enable billing on the key's Google Cloud project, or stay
    on the default `gemini-3.5-flash`.

- Transcription fails with `400 JSON mode is not enabled for this model`:
  - The model in `GEMINI_TRANSCRIBE_MODEL` does not support structured output. The
    dedicated `gemini-*-transcribe` models fall in this group; use a flash/pro model.

- Caption rewrite key error (`OPENAI_API_KEY is not set`):
  - `/rewrite-captions` always needs it, and so does transcription when an OpenAI model
    is picked; Gemini models do not.

## Notes

- Uploaded videos are stored in `videos/`.
- Extracted frames are stored in `frames/`.
- Generated transcripts are stored in `texts/`.
- Database schema is created automatically by backend startup.
- If `FRAME_EXTRACTOR_URL` is unset, the backend falls back to the original in-process Python frame extraction path.
