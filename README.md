# Video ASR Pipeline

Full-stack video service for uploading videos, generating Thai transcripts with timestamped captions, and extracting frames. Any department can use it directly — there is no course/project grouping layer.

## Tech Stack

- Frontend: Next.js 16 + React 19
- Backend: FastAPI + SQLAlchemy
- Database: PostgreSQL 15
- Media processing: FFmpeg, with frame extraction offloaded to a Rust service in Docker
- AI: OpenAI speech-to-text (`gpt-transcribe`, `whisper-1`, ...), picked per run; OpenAI `gpt-5` for optional caption proofreading
- Thai text processing: PyThaiNLP (`newmm`) for word boundaries when splitting caption lines

## Project Structure

```text
.
|-- api.py                  # FastAPI application and endpoints
|-- database.py             # SQLAlchemy engine/session setup
|-- models.py               # Database models: User, Project, Video, GlossaryRule
|-- pipeline.py             # Audio extraction, transcription, SRT/VTT captions, frame extraction adapter
|-- glossary.py             # The word system: ASR error rules + one-pass corrector
|-- caption_polish.py       # Line splitting, spacing repair, timestamp sanitising
|-- pricing.py              # Per-run time and cost estimates
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
- Transcribe video audio to Thai text with per-segment timestamps, choosing the model per run
- See the projected time and cost *before* transcribing, and the actual figures after
- Fix ASR errors from a persistent word system in a single pass, with every change highlighted
- Split long blocks into short subtitle lines (~42-52 chars) on real Thai word boundaries
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
# Transcription and caption rewriting both run on OpenAI. This is the only AI key.
OPENAI_API_KEY=your_openai_api_key

# Default model for the picker.
# TRANSCRIBE_MODEL=gpt-transcribe
# Extra ids for the UI model picker, comma-separated.
# OPENAI_TRANSCRIBE_MODELS=gpt-4o-transcribe-diarize
# Chat model for the optional LLM proofreader.
# REWRITE_MODEL=gpt-5
# Override the built-in price list behind the per-run time/cost estimate.
# TRANSCRIBE_PRICING={"gpt-transcribe":{"per_minute":0.006,"realtime_factor":14}}

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
- `GET /api/transcribe-models` - transcription models the UI can offer, with per-minute cost
- `GET /api/videos/{video_id}/transcribe-estimate?model=<id>` - projected time and cost, before running
- `POST /api/videos/{video_id}/transcribe` - generate transcript + caption segments (optional body: `{"model": "<id>", "polish": true}`)
- `POST /api/videos/{video_id}/polish-captions` - re-run the free clean-up pass (optional body: `{"split_lines": true, "min_chars": 42, "max_chars": 52}`)
- `POST /api/videos/{video_id}/rewrite-captions` - optional paid LLM proofread, then the clean-up pass
- `GET /api/videos/{video_id}/captions.vtt` - WebVTT captions (for the video `<track>` element)
- `GET /api/videos/{video_id}/captions.srt` - SRT captions (download)
- `PATCH /api/videos/{video_id}/captions/{segment_index}` - edit one caption line (`{"text": "..."}`)
- `GET /api/glossary` - the compiled word system: rules, protected terms, counts
- `POST /api/glossary` - add or override a rule (`{"right": "best.pt", "wrong": ["base.pt"], "category": "filename"}`)
- `PATCH /api/glossary/{rule_id}` - enable/disable a rule, built-ins included (`{"enabled": false}`)
- `DELETE /api/glossary/{rule_id}` - remove a rule you added
- `POST /api/glossary/preview` - try the word system on one line (`{"text": "..."}`)

Interactive docs:

- Swagger UI: `/docs`
- ReDoc: `/redoc`

## Transcription Models

The model is chosen per run from the picker on the video page; `GET /api/transcribe-models`
returns the list (empty when `OPENAI_API_KEY` is unset).

| Model | Timestamps | Notes |
|---|---|---|
| `gpt-transcribe` | derived | Default. Most accurate on Thai in our testing. |
| `gpt-4o-transcribe` | derived | Previous generation. |
| `gpt-4o-mini-transcribe` | derived | Cheapest. |
| `whisper-1` | native | Weakest text; returns its own segment timings. |

The `gpt-*transcribe` models reject `response_format=verbose_json` and return text with
no timings at all. For those, `pipeline._silence_cue_bounds()` cuts the audio into
2-12s windows in the middle of pauses found by ffmpeg `silencedetect`, transcribes each
window separately, and takes the cue timing from the cut points.

## Caption Clean-up

After every transcription a deterministic pass runs over the captions. It is free, needs
no API key, and is idempotent — re-running it on already-clean captions changes nothing.
Trigger it again from **Polish** on the video page, or `POST /api/videos/{id}/polish-captions`.

It does four things, in order:

1. **ASR error correction (one pass).** `glossary.py` compiles every rule and every
   protected term into a *single* alternation regex, so one left-to-right pass fixes
   every occurrence of every misheard spelling. Because a replacement is never
   re-examined, rules cannot cascade into each other; the longest variant always wins.
   Covered out of the box: spoken punctuation (`ตกใจ` → `!`), player buttons
   (`เพย์`/`พอส`/`Post` → `Play`/`Pause`), file names (`base.pt` → `best.pt`,
   `data.yml` → `data.yaml`), Colab paths (`content/run/detect/train` →
   `/content/runs/detect/train`), platform names (`Robo4Universe` → `Roboflow Universe`),
   logic gates (`แอนด์เกต` → `AND Gate`), YOLO sizes (`นาโน` → `Nano`), and technical
   terms that should read as capitalised English (`แมชชีนเลิร์นนิ่ง` → `Machine Learning`).
2. **Protected Thai words.** `โมเดล`, `โมดูล`, `เทรน`, `เซฟ`, `พาธ`, `แพทเทิร์น` and
   friends stay Thai. A protected term consumes its own text in that same pass, so no
   rule can fire inside it either.
3. **Spacing repair.** Replacing a transliteration with English leaves the scripts glued
   together; `เทรนmodel` becomes `เทรน model`.
4. **Line splitting and timing.** Long blocks are cut into ~42-52 character lines on real
   Thai word boundaries (PyThaiNLP `newmm`, plus a domain dictionary so `เทรน` is not
   split into `เท`+`รน`). Breaks prefer a full stop or a Thai sentence particle, then a
   space, then the start of a connector, and never fall inside a multi-word English term
   like `Working Directory`. A block already within budget is left alone. Each block's
   time is divided across its lines in proportion to their length, then every cue is
   checked so none overlaps and none has zero duration.

Every replacement is reported back and highlighted in the cue list, aggregated by rule
(`best.pt ← base.pt ×3`) in the report panel.

### The word system

Manage it from **ระบบคำ** in the header. You can add a rule, override a built-in by
re-using its canonical spelling, disable any rule (built-ins included), mark a word as
protected so nothing converts it, and try the whole system against a sample line before
relying on it. Built-in rules live in `glossary.py`; your changes are rows in
`glossary_rules` and are scoped to your user.

## Cost and Time Reporting

Every run reports both. Before transcribing, the video page shows the projected duration
and cost for the selected model (`GET /api/videos/{id}/transcribe-estimate`). Afterwards
it shows the measured wall-clock time, the realtime factor, the request count and the
estimated spend, stored on the video as `transcribe_stats`. The optional LLM proofread
reports separately as `rewrite_stats`, priced from the token counts the API returns.

Prices come from `pricing.py` and are **estimates for budgeting, not a bill** — override
them with `TRANSCRIBE_PRICING` when published rates move. The CLI prints the same figures,
and `--estimate-only` prints just the projection:

```bash
python pipeline.py video.mp4 --estimate-only
python pipeline.py video.mp4            # transcribe + polish, then print time/cost
python pipeline.py video.mp4 --no-polish
```

## Typical Workflow

1. Upload a video from the homepage.
2. Open the video page and pick a transcription model — the projected time and cost appear
   beneath the button.
3. Trigger transcription. The word system, line splitting and timing repair run
   automatically; the actual time and cost appear when it finishes.
4. Open the report panel to see every word that was corrected, grouped by rule.
5. Preview the synced caption list, or toggle native subtitles on the player.
6. Click the pencil (or double-click a cue) to hand-fix any remaining line. If a word was
   mis-transcribed in a way the system missed, add it in **ระบบคำ** and hit **Polish**
   so it is fixed everywhere at once.
7. Extract frames as needed.

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

- Key error (`OPENAI_API_KEY is not set`), or the model picker is empty:
  - Ensure `.env` exists in the project root and holds a valid OpenAI API key.
    Transcription and `/rewrite-captions` both need it; `/polish-captions` does not.

- Captions are split on odd boundaries, and the report says `tokenizer: fallback`:
  - PyThaiNLP failed to import, so the splitter is using its coarse regex fallback.
    Reinstall dependencies (`pip install -r requirements.txt`) and restart the backend.

- A word keeps being corrected when it should not be (or vice versa):
  - Open **ระบบคำ** in the header. Disable the offending rule, or add the word as a
    protected term so no rule may touch it.

## Notes

- Uploaded videos are stored in `videos/`.
- Extracted frames are stored in `frames/`.
- Generated transcripts are stored in `texts/`.
- Database schema is created automatically by backend startup.
- If `FRAME_EXTRACTOR_URL` is unset, the backend falls back to the original in-process Python frame extraction path.
