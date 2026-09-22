# Video ASR Pipeline

Full-stack video service for uploading videos, generating Thai transcripts with timestamped captions, and extracting frames. Any department can use it directly — there is no course/project grouping layer.

## Tech Stack

- Frontend: Next.js 16 + React 19
- Backend: FastAPI + SQLAlchemy
- Database: PostgreSQL 15
- Media processing: FFmpeg, with frame extraction and auto trim offloaded to Rust services in Docker
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
|-- fcpxml.py               # Editor projects for an auto-trimmed edit (FCPXML and FCP7 XML)
|-- timeline_json.py        # auto-editor's own timeline JSON for the same edit
|-- pricing.py              # Per-run time and cost estimates
|-- docker-compose.yml      # Multi-service local stack
|-- docker-compose.vaapi.yml   # Overlay: pass an Intel/AMD GPU to the trim service
|-- docker-compose.nvidia.yml  # Overlay: pass an NVIDIA GPU to the trim service
|-- Dockerfile              # Backend image
|-- frame-extractor-rs/     # Rust microservice for frame extraction
|-- auto-trim-rs/           # Rust microservice for auto trim (auto-editor's cut, ported)
|-- frontend/               # Next.js app
|-- videos/                 # Uploaded video files
`-- texts/                  # Generated transcript and .srt files
```

## Features

- Upload videos directly, no grouping required
- Stream uploaded videos
- Extract evenly spaced frames from videos
- Cut the silent parts out of a video with one button, using auto-editor's edit decision
- See the loudness envelope before and after the cut, and export the edit as an
  editor project — FCPXML, Final Cut Pro 7 XML (Premiere, Resolve) or auto-editor's
  own timeline JSON — still pointing at the original footage
- Transcribe video audio to Thai text with per-segment timestamps, choosing the model per run
- See the projected time and cost *before* transcribing, and the actual figures after
- Fix ASR errors from a persistent word system in a single pass, with every change highlighted
- Split long blocks into short subtitle lines (~42-52 chars) on real Thai word boundaries
- Preview synced captions alongside video playback (click-to-seek cue list + native subtitle track)
- Edit any caption line by hand to fix ASR typos
- Find & replace across the captions (Ctrl+F), one line at a time or all at once,
  keeping a before/after history and optionally teaching the pair to the word system

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
# AUTO_TRIM_URL=http://localhost:8082
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

- `POST /api/videos?filename=<name>&project_id=<id>` - upload a video: the request body is the
  raw file bytes (not multipart), streamed straight to disk — see [Uploads](#uploads)
- `GET /api/videos` - list all videos
- `GET /api/videos/{video_id}` - get video
- `GET /api/videos/stream/{filename}` - stream video
- `POST /api/videos/{video_id}/frames` - extract frames for a video
- `GET /api/videos/{video_id}/frames` - list extracted frames
- `GET /api/videos/{video_id}/frames/{frame_filename}` - fetch an extracted frame
- `GET /api/auto-trim/capabilities` - the CPU, the GPU devices in the trim service's container, and every encoder it tried
- `GET /api/videos/{video_id}/auto-trim/estimate?kept_ratio=` - how long a trim would take (instant)
- `POST /api/videos/{video_id}/auto-trim/preview` - start an analyze-only job: what would be cut
- `POST /api/videos/{video_id}/auto-trim` - start the render of the video without its silent parts
- `GET /api/videos/{video_id}/auto-trim/job` - phase, progress and ETA of that job
- `DELETE /api/videos/{video_id}/auto-trim/job` - cancel it, killing the encodes
- `GET /api/videos/{video_id}/auto-trim/fcpxml?media_path=&version=11` - the edit as a Final Cut Pro project
- `GET /api/videos/{video_id}/auto-trim/xml?media_path=` - the edit as Final Cut Pro 7 XML (Premiere, Resolve)
- `GET /api/videos/{video_id}/auto-trim/json?version=3&media_path=` - the edit as an auto-editor timeline
- `GET /api/videos/{video_id}/trimmed?download=1` - stream or download the trimmed render
- `DELETE /api/videos/{video_id}/trimmed` - discard the trimmed render
- `GET /api/transcribe-models` - transcription models the UI can offer, with per-minute cost
- `GET /api/videos/{video_id}/transcribe-estimate?model=<id>` - projected time and cost, before running
- `POST /api/videos/{video_id}/transcribe` - generate transcript + caption segments (optional body: `{"model": "<id>", "polish": true}`)
- `POST /api/videos/{video_id}/polish-captions` - re-run the free clean-up pass (optional body: `{"split_lines": true, "min_chars": 42, "max_chars": 52}`)
- `POST /api/videos/{video_id}/rewrite-captions` - optional paid LLM proofread, then the clean-up pass
- `GET /api/videos/{video_id}/captions.vtt` - WebVTT captions (for the video `<track>` element)
- `GET /api/videos/{video_id}/captions.srt` - SRT captions (download)
- `PATCH /api/videos/{video_id}/captions/{segment_index}` - edit one caption line (`{"text": "..."}`)
- `POST /api/videos/{video_id}/captions/replace` - find & replace over the caption lines
  (`{"find": "...", "replace": "...", "match_case": false, "segment_indexes": null, "save_to_glossary": true}`)
- `GET /api/videos/{video_id}/captions/replacements` - every find & replace run on this video, newest first
- `GET /api/glossary` - the compiled word system: rules, protected terms, counts
- `POST /api/glossary` - add or override a rule (`{"right": "best.pt", "wrong": ["base.pt"], "category": "filename"}`)
- `PATCH /api/glossary/{rule_id}` - enable/disable a rule, built-ins included (`{"enabled": false}`)
- `DELETE /api/glossary/{rule_id}` - remove a rule you added
- `POST /api/glossary/preview` - try the word system on one line (`{"text": "..."}`)

Interactive docs:

- Swagger UI: `/docs`
- ReDoc: `/redoc`

## Uploads

Pick several files at once from **Upload Videos**, or drop them anywhere on the header — both
paths accept multiple files and upload up to 4 at a time concurrently, each with its own
progress bar; a failed one shows why and stays until dismissed, a finished one clears itself.

`POST /api/videos` takes the file as the raw request body instead of a multipart form, with
the name and target project passed as query params (`?filename=<name>&project_id=<id>`). This
is what makes uploads fast: the old multipart endpoint spooled the file to a temp location
first and then copied *that* into `videos/` — every upload touched disk twice — and did the
copy with a blocking call inside an `async def`, which stalled the entire worker (every other
request on it, uploads included) until it finished. Streaming the body directly is a single
disk write, and awaiting it chunk by chunk hands control back between chunks, so uploads —
concurrent or not — no longer block each other or anything else the server is doing.

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

## Auto Trim

**ตัดช่วงเงียบอัตโนมัติ** on the video page cuts the silent parts out of a video and
renders what is left as a new file. The upload is never modified: the result is a
separate render you can play, download, re-run with other settings, or throw away.

Videos up to **three hours** are accepted (`MAX_DURATION_SECONDS`), with no limit on
file size — a 980 MB source is nothing unusual and nothing in the path loads a whole
track or picture into memory.

### Three buttons and a bar

- **วิเคราะห์** (`POST /api/videos/{id}/auto-trim/preview`) starts an analyze-only
  job: what would be cut, without the encode time.
- **ตัดอัตโนมัติ** (`POST /api/videos/{id}/auto-trim`) starts the render.
- **ยกเลิก** (`DELETE /api/videos/{id}/auto-trim/job`) stops a running job and kills
  the ffmpeg processes it has going.

Both start calls return straight away with a job id, and the UI polls
`GET /api/videos/{id}/auto-trim/job` for phase, progress and ETA — a three-hour
video takes minutes, which is far too long to hold a request open for. The id is
kept on the video row, so closing the tab does not lose the render: the next poll,
from any worker, files the result.

The bar's pace is weighted by each phase's *expected* time rather than its frames,
so it moves evenly instead of crawling through the encode, and the render phase
tracks whichever of picture and audio is **behind** — they run at once, and the
phase is only over when the slower one lands. The ETA comes from the up-front
estimate until about 5% is done, then from the measured pace.

`GET /api/videos/{id}/auto-trim/estimate` answers instantly (ffprobe only) with how
long a trim should take. Without `kept_ratio` it assumes the whole video survives,
which makes it an upper bound; the UI passes the share a preview measured to sharpen
it. The rates behind it (audio analysis, pixels per second, audio encoding)
calibrate themselves from every run, so the numbers fit the machine rather than a
guess baked into the code.

### Seeing the cut

The panel draws the loudness envelope twice: **ก่อนตัด**, the whole timeline with
the parts that will go shaded red and the threshold marked, and **หลังตัด**, what
the trimmed file sounds like end to end. Clicking either one seeks the matching
player, so a suspicious cut can be listened to before committing to it.

The drawing comes from the same levels the edit is decided from — summarised to
1200 buckets, peak per bucket (an average would smear a quiet gap into the speech
beside it, hiding exactly the distinction being shown). Alongside it the service
sends how much of each bucket survives, which is what the red bands are drawn
from: 1.2 kB whether the edit has four cuts or four thousand, so the picture is
still there after a reload without keeping a segment list in the row.

### Exporting the edit to an editor

Two buttons next to the video download, for the two XMLs editors read:

| Button | Endpoint | Reads it |
|---|---|---|
| **FCPXML** | `/auto-trim/fcpxml` | Final Cut Pro 10.6.8 and later (`?version=10` for a little older) |
| **XML** | `/auto-trim/xml` | Premiere Pro, DaVinci Resolve, Final Cut Pro 7 |
| **JSON** | `/auto-trim/json` | auto-editor itself, and anything that wants the cut list as data |

Either way the XML references the **original** upload, not the trimmed render, so
the editor opens a project whose cuts are already made but still adjustable —
which is the point of taking an edit into an editor at all. Each kept range
becomes one clip carrying the source's picture and audio together.

The two count time differently, and both have a trap:

- FCPXML uses rationals with the timeline's own denominator
  (`{frames × 1001}/30000s` for 29.97). Decimals here are what make an imported
  timeline drift off the frame the edit chose.
- The Final Cut Pro 7 format counts whole frames on a *rounded* timebase with an
  NTSC flag — 29.97 is "timebase 30, ntsc TRUE". Frame numbers stay exact; the
  flag is what tells the editor how long a frame is. A stereo source explodes into
  one audio track per channel, each clip linked to its picture so they move
  together, which is what Premiere expects.

Editors relink media they cannot find, so both files import as-is and ask where the
footage lives. Filling in the footage folder next to the buttons (or
`?media_path=/Volumes/Work/footage`) spares that step.

[`fcpxml.py`](fcpxml.py) writes both, shaped after auto-editor's
`src/exports/fcp11.nim` and `src/exports/fcp7.nim` — the reference for what these
apps actually accept. [`test_fcpxml.py`](test_fcpxml.py) covers the parts that fail
silently: frame rationals, NTSC flags, clips sitting end to end, one file
definition referenced by every clip, links that resolve, Thai filenames as both URL
and XML, and the two documents agreeing on the same edit.

### Exporting the cut list as data

**JSON** writes auto-editor's own timeline ([`timeline_json.py`](timeline_json.py)),
which goes back where the XMLs cannot: `auto-editor timeline.json -o out.mp4`
re-renders from it, and a script can read it without parsing XML.

`?version=3` (the default) is the full timeline — resolution, sample rate, layout,
one clip per kept range on a video layer and an audio layer per stream, exactly what
`--export json` writes. `?version=1` is the compact cut list: every chunk of the
timeline in order, kept ones at speed 1 and cut ones at auto-editor's 99999
("drop this"), so nothing is left implicit. Here `src` is a filesystem path rather
than a `file://` URL, because that is what auto-editor opens.

### Verified against auto-editor itself

The released auto-editor (29.3.1 from PyPI) was given the same file and asked for
its own decision, and our exports were handed back to it:

- It **imported both JSON timelines and re-rendered them**: 523 frames each, against
  the 523 frames our own renderer produced.
- Feeding *its* per-frame levels through our ported mask reproduces its cut
  boundaries exactly — `(0,24) (34,146) (147,395) (410,522) (1194,1220)` — so the
  loudness analysis and the margin agree frame for frame across all 1272 frames.
- Its own edit keeps 522 frames where ours keeps 523: a single frame, and not a
  discrepancy. The vendored source tree is 31.6.1, which added the `--smooth`
  pass (mincut/minclip) that fills a one-frame gap at frame 146; the 29.3.1 release
  has no `--smooth` at all. Our port follows the tree it was ported from.

### The edit decision is auto-editor's

The decision of *which frames to keep* is a port of [auto-editor](https://auto-editor.com)
(`--edit audio`), not a reimplementation of the idea:

1. The timeline runs at the source's frame rate, rounded to two decimals, with the
   NTSC rates restored to their exact rationals (`makeSaneTimebase`).
2. Audio is decoded to interleaved s16 at its own sample rate, and each timeline
   frame gets one level: the loudest sample in that frame's slice, as a fraction of
   full scale. Frame sizes are `sample_rate / timebase` with the rounding error
   carried forward, so the slices stay aligned to the clock over a long file
   (`src/analyze/audio.nim`).
3. Levels and the threshold are compared as 16-bit fixed point, the way auto-editor
   stores them (`Unorm16`), so a borderline frame lands on the same side in both.
4. Every active run is grown by the margin at each end (`mutMargin`), then short
   runs are smoothed away (`smoothing`): silences shorter than `mincut` are filled,
   clips shorter than `minclip` are dropped, repeatedly until the mask stops
   changing.
5. Each run of kept frames becomes one clip, back to back, with no speed change.

Steps 4 and 5 live in [`auto-trim-rs/src/mask.rs`](auto-trim-rs/src/mask.rs) with
unit tests that pin the quirks (a leading active run has no transition for the start
margin to grow from; the final run is measured inclusively; the all-flip 2-cycle
exits on the input).

Analysis is deterministic, so its levels are cached per (file, timebase, stream).
Re-running a preview at a different threshold skips it entirely: measured 57.9s to
0.05s on a three-hour file.

### Settings

| Setting | Default | auto-editor equivalent |
|---|---|---|
| ความดังขั้นต่ำ (`threshold`) | 4% | `--edit audio:threshold=0.04` |
| เผื่อหัวท้าย (`margin_start`, `margin_end`) | 0.2s | `--margin 0.2s` |
| `mincut` | 0.2s | `--smooth 0.2s,0.1s` |
| `minclip` | 0.1s | `--smooth 0.2s,0.1s` |
| `crf`, `preset` | 20, `veryfast` | encoder only, not part of the decision |

`mincut`/`minclip` and the encoder knobs are API-only; the UI exposes the two
settings worth touching per video. Raise the threshold to cut more aggressively,
raise the margin if the cuts feel abrupt.

### How the render works

The picture is encoded in **chunks that run in parallel**, each ffmpeg decoding only
its own span of the source and filtering it down to the surviving frames with
`select`, then rebuilding the timestamps with `setpts` at the timeline's timebase.
The chunks are joined with a stream copy and muxed with the audio.

Chunk planning balances kept footage across the encode slots, but also:

- caps the segments per chunk (`MAX_SEGMENTS_PER_CHUNK`, 64), because a `select`
  expression is evaluated for every frame;
- keeps chunks at least `MIN_CHUNK_SECONDS` (20s) of kept footage, since each one
  pays for its own seek and decode lead-in;
- breaks before a gap longer than `MAX_GAP_SECONDS` (10s), because a chunk decodes
  its whole span, cut-away parts included.

The `select` terms are summed as a **balanced tree**, not a flat `a+b+c+…` chain.
FFmpeg's expression parser descends recursively with a fixed budget of 100 levels,
and a flat chain spends it in proportion to the number of terms: about 135 cuts is
enough to fail the parse outright with "Cannot allocate memory". A tree needs only
log2(n) levels. (Verified directly: 139 terms flat fails, 139 and even 2000 as a
tree parse fine.)

Audio is **not** cut by ffmpeg. `aselect` can only cut on packet boundaries (~21ms
at 48kHz), and rounding every cut to a packet would drift out of sync with the
picture a little on every one of them — seconds of drift across the hundreds of cuts
a long video produces. Instead the service splices the samples itself, on the exact
frame boundaries the analysis used, and streams them into one encoder (so there is a
single encoder priming block and no drift at the joins). Decoding, splicing and
encoding run on separate threads, and progress comes from the encoder's own report
rather than from what has been fed to it.

Every render is checked against the edit: the frames ffmpeg reports across all
chunks must match the frames the mask keeps, or the job fails rather than writing a
file that quietly dropped content.

### Measured

On a 16-core machine with no GPU passed into the container:

| Source | Analyze | Render | Total |
|---|---|---|---|
| 42s, 720p, 4 segments | 0.2s | 1.8s | 2.0s |
| 20 min, 320x240, 120 segments | 1.7s (0.05s cached) | 14.6s | 16.3s |
| 10 min, 1080p, 980 MB, 66 segments | 2.2s | 115.1s | 117.3s |
| 3 h, 320x240, 1080 segments | 12-58s (0.05s cached) | — | — |

Frame counts came back exact in every case. The 1080p figure is a worst case for the
encoder: the test source is pure noise, which is far harder to compress than real
footage.

### CPU and GPU detection

At startup the service works out what the machine can encode with, and uses a GPU
whenever one is usable. Nothing is taken on trust: ffmpeg being *built* with
`h264_nvenc` says nothing about whether the container can reach a GPU — the Debian
build advertises NVENC, QSV and VAAPI on a laptop with no `/dev/dri` at all. So each
candidate is tried against a real clip, **through the same filter chain a render
uses**, and only one that produces frames is chosen.

Each family is tried in two shapes, fastest first:

1. **GPU decode + GPU encode** — `-hwaccel cuda|qsv|vaapi` with
   `-hwaccel_output_format`, so frames never leave the device. `select` and `setpts`
   only pass frames along without touching pixels, which is what makes a full-GPU
   pipeline possible at all.
2. **CPU decode + GPU encode** — the encoder uploads (VAAPI gets an explicit
   `format=nv12,hwupload` on the end of the chain).

Order: NVENC, Quick Sync, VAAPI, then `libx264`. `VIDEO_CODEC` forces one (still
tested, still falling back if it cannot run). The chosen encoder also seeds the time
estimate, so the first estimate on a GPU box is not a CPU-shaped guess.

`GET /api/auto-trim/capabilities` reports the whole finding — and the trim panel
shows a **CPU/GPU badge** with the encoder in use:

```json
{
  "cpu_model": "AMD Ryzen 7 8840U w/ Radeon 780M Graphics",
  "cpu_cores": 16,
  "devices": [],
  "built_with": ["h264_nvenc", "h264_qsv", "h264_vaapi", "..."],
  "chosen": "libx264",
  "hardware": false,
  "pipeline": "cpu decode + cpu encode",
  "attempts": [
    {"encoder": "h264_nvenc", "pipeline": "gpu decode + gpu encode (nvenc)",
     "ok": false, "detail": "Device setup failed for decoder ... Operation not permitted"}
  ]
}
```

`devices: []` with a GPU in `cpu_model` is the usual story: the machine has one, the
container cannot see it.

### Giving the container a GPU

A hardware encoder is the cheapest large speed-up available (5-15x on the render),
and the render is the whole cost of a trim. It needs the device passed in:

```bash
# Intel or AMD graphics (VAAPI / Quick Sync)
docker compose -f docker-compose.yml -f docker-compose.vaapi.yml up -d

# NVIDIA (needs the NVIDIA Container Toolkit on the host)
docker compose -f docker-compose.yml -f docker-compose.nvidia.yml up -d
```

Then check it took:

```bash
curl -s localhost:8734/api/auto-trim/capabilities -H "Authorization: Bearer $TOKEN"
```

Docker Desktop on Windows and macOS cannot pass a GPU through for VAAPI/QSV, so a
dev machine there stays on `libx264` — which is exactly what the report will say,
rather than failing halfway through a render.

### Not (yet) smart render

auto-editor can copy whole GOPs and re-encode only the partial ones at each edit
boundary ([`src/render/smart.nim`](auto-editor/src/render/smart.nim)), which would
cut the encode by 2-8x depending on the source's keyframe interval, with no quality
loss on the copied parts. It is not here yet because it cannot be assembled through
the ffmpeg CLI — four approaches were measured on a 360-frame edit:

| Assembly | Frames (want 360) | Copied spans |
|---|---|---|
| concat demuxer + `inpoint`/`outpoint` | 364 | misplaced |
| TS pieces + `-to` | 364 | misplaced |
| TS pieces + `-frames:v` | 360 | one span lost its first frame to a DTS collision |
| raw Annex B + `cat` | 360 packets, 358 decoded | **bit-identical and correctly placed** |

The pieces themselves work — copied GOPs come out bit-identical, and mixed SPS/PPS
is fine in band — but the timestamps at each join have to be written per packet,
which is why auto-editor links libav instead of shelling out. Doing it here means
the same: `ffmpeg-next` plus a packet-level assembler.

### Service

`auto-trim-rs` is a Rust (axum) service in the compose stack, same shape as
`frame-extractor-rs`: it shares the `videos_data` volume and the backend reaches it
over `AUTO_TRIM_URL`. There is deliberately no Python fallback — a second
implementation of the edit decision would drift from this one, and a video trimmed
differently depending on deployment is worse than a clear error.

| Env var | Where | Default | Meaning |
|---|---|---|---|
| `AUTO_TRIM_URL` | backend | set by compose | `http://auto-trim:8082`; unset makes the endpoints answer 503 |
| `AUTO_TRIM_TIMEOUT` | backend | 120 | seconds to wait on a call (all of them are quick now) |
| `MAX_DURATION_SECONDS` | auto-trim | 10800 | longest video accepted |
| `MAX_ACTIVE_JOBS` | auto-trim | 4 | jobs allowed to run at once (a 5th gets a 429) |
| `ENCODE_WORKERS` | auto-trim | cores ÷ threads-per-chunk | parallel chunk encodes, shared by every job |
| `THREADS_PER_CHUNK` | auto-trim | 2 | threads per chunk encode |
| `MAX_SEGMENTS_PER_CHUNK` | auto-trim | 64 | bounds the per-frame filter cost |
| `MIN_CHUNK_SECONDS` | auto-trim | 20 | smallest worthwhile chunk |
| `MAX_GAP_SECONDS` | auto-trim | 10 | gap that breaks a chunk |
| `VIDEO_CODEC` | auto-trim | detected | force an encoder (`libx264`, `h264_nvenc`, `h264_qsv`, `h264_vaapi`) |
| `VAAPI_DEVICE` | auto-trim | `/dev/dri/renderD128` | which render node VAAPI uses |
| `PRESET`, `CRF` | auto-trim | `veryfast`, 20 | quality, translated per encoder family (`-cq` for NVENC, `-global_quality` for QSV, `-qp` for VAAPI) |
| `AAC_CODER` | auto-trim | `fast` | ffmpeg's default (`twoloop`) is half the speed at the same bitrate |
| `LEVEL_CACHE`, `LEVEL_CACHE_MB` | auto-trim | on, 2048 | analysis cache and its size cap |
| `JOB_RETENTION_SECONDS` | auto-trim | 7200 | how long a finished job stays pollable |
| `WORK_DIR` | auto-trim | `/tmp/auto-trim` | scratch space and the level cache |

Errors keep their meaning: a file with no audio track answers 400, one over the
length limit answers 400 with its length, an edit that would keep nothing answers
422, too many jobs answers 429 — and a failed or cancelled run leaves any previous
render untouched.

### Not included

Captions are not re-timed against the cut. The trimmed render and the transcript of
the original no longer line up, so transcribe *after* trimming if you need both.
(Both exports have the same boundary: they carry picture and audio, not
subtitles.)

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

### Find & replace

**ค้นหา/แทนที่** (or Ctrl+F) on the video page searches the caption lines and marks every
hit. The match count updates as you type; Enter and Shift+Enter walk between the lines that
matched, and **Aa** makes the search case-sensitive. Replace everything at once with
**แทนที่ทั้งหมด**, or use the per-line button to change a single cue.

Every run is saved to `caption_replacements` as the word before and after, how many places
changed, and when — the history panel under the search box lists them newest first, and
`GET /api/videos/{id}/captions/replacements` returns the same rows. Leaving
**บันทึกคู่คำเข้าระบบคำ** checked also teaches the pair to the word system, so the next
transcript fixes it without being asked. An existing rule for that spelling is extended
rather than duplicated, whatever category it sits in.

Replacement is literal, not a regex, and a change that would leave a caption line empty is
refused rather than producing a broken cue.

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
2. To cut the dead air out first, run **วิเคราะห์** to see how much would go, then
   **ตัดอัตโนมัติ**. Transcribe after trimming, not before: captions are not re-timed
   against the cut.
3. Open the video page and pick a transcription model — the projected time and cost appear
   beneath the button.
4. Trigger transcription. The word system, line splitting and timing repair run
   automatically; the actual time and cost appear when it finishes.
5. Open the report panel to see every word that was corrected, grouped by rule.
6. Preview the synced caption list, or toggle native subtitles on the player.
7. Click the pencil (or double-click a cue) to hand-fix any remaining line. If the same
   wrong word appears in several places, hit Ctrl+F and replace it everywhere at once —
   leave **บันทึกคู่คำเข้าระบบคำ** checked and the next transcript fixes it by itself.
8. Extract frames as needed.

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
