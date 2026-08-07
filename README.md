# Video ASR Pipeline

Full-stack video service for uploading videos, generating Thai transcripts with timestamped captions, and extracting frames. Any department can use it directly — there is no course/project grouping layer.

## Tech Stack

- Frontend: Next.js 16 + React 19
- Backend: FastAPI + SQLAlchemy
- Database: PostgreSQL 15
- Media processing: FFmpeg, with frame extraction offloaded to a Rust service in Docker
- AI: OpenAI (`whisper-1`, for timestamped Thai transcription)

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
- Transcribe video audio to Thai text with per-segment timestamps
- Preview synced captions alongside video playback (click-to-seek cue list + native subtitle track)

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
- `POST /api/videos/{video_id}/transcribe` - generate transcript + caption segments
- `GET /api/videos/{video_id}/captions.vtt` - WebVTT captions (for the video `<track>` element)
- `GET /api/videos/{video_id}/captions.srt` - SRT captions (download)

Interactive docs:

- Swagger UI: `/docs`
- ReDoc: `/redoc`

## Typical Workflow

1. Upload a video from the homepage.
2. Open the video page and trigger transcription.
3. Preview the synced caption list, or toggle native subtitles on the player.
4. Extract frames as needed.

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

- OpenAI key error (`OPENAI_API_KEY is not set`):
  - Ensure `.env` exists in root and contains a valid key.

## Notes

- Uploaded videos are stored in `videos/`.
- Extracted frames are stored in `frames/`.
- Generated transcripts are stored in `texts/`.
- Database schema is created automatically by backend startup.
- If `FRAME_EXTRACTOR_URL` is unset, the backend falls back to the original in-process Python frame extraction path.
