import logging
import os
import re
import shutil
import time
import io
import zipfile
from datetime import datetime, timezone
from pathlib import Path
from fastapi import FastAPI, HTTPException, Depends, Query, Request, UploadFile, File
from fastapi.concurrency import run_in_threadpool
from fastapi.responses import FileResponse, Response, StreamingResponse
from fastapi.middleware.cors import CORSMiddleware
from fastapi.security import OAuth2PasswordRequestForm
from sqlalchemy import func, text
from sqlalchemy.orm import Session
from dotenv import load_dotenv
from typing import List, Optional
from pydantic import BaseModel

import pipeline
import caption_polish
import glossary as glossary_module
import models
import auth
from database import engine, get_db

load_dotenv()

logging.basicConfig(level=os.getenv("LOG_LEVEL", "INFO"))
logger = logging.getLogger("api")

app = FastAPI()


@app.on_event("startup")
def init_database():
    max_attempts = 20
    for attempt in range(1, max_attempts + 1):
        try:
            with engine.connect() as conn:
                conn.execute(text("SELECT 1"))
            models.Base.metadata.create_all(bind=engine)
            with engine.begin() as conn:
                conn.execute(
                    text(
                        "ALTER TABLE videos ADD COLUMN IF NOT EXISTS owner_id "
                        "INTEGER REFERENCES users(id)"
                    )
                )
                conn.execute(
                    text("ALTER TABLE users ADD COLUMN IF NOT EXISTS reset_token_hash VARCHAR")
                )
                conn.execute(
                    text(
                        "ALTER TABLE users ADD COLUMN IF NOT EXISTS reset_token_expires_at "
                        "TIMESTAMPTZ"
                    )
                )
                conn.execute(
                    text(
                        "ALTER TABLE videos ADD COLUMN IF NOT EXISTS is_deleted "
                        "BOOLEAN NOT NULL DEFAULT FALSE"
                    )
                )
                conn.execute(
                    text("ALTER TABLE videos ADD COLUMN IF NOT EXISTS deleted_at TIMESTAMPTZ")
                )
                conn.execute(
                    text(
                        "ALTER TABLE videos ADD COLUMN IF NOT EXISTS project_id "
                        "INTEGER REFERENCES projects(id)"
                    )
                )
                conn.execute(
                    text("ALTER TABLE videos ADD COLUMN IF NOT EXISTS transcribe_stats JSONB")
                )
                conn.execute(
                    text("ALTER TABLE videos ADD COLUMN IF NOT EXISTS polish_report JSONB")
                )
                conn.execute(
                    text("ALTER TABLE videos ADD COLUMN IF NOT EXISTS rewrite_stats JSONB")
                )
                position_exists = conn.execute(
                    text(
                        "SELECT 1 FROM information_schema.columns "
                        "WHERE table_name = 'videos' AND column_name = 'position'"
                    )
                ).first()
                conn.execute(
                    text(
                        "ALTER TABLE videos ADD COLUMN IF NOT EXISTS position "
                        "INTEGER NOT NULL DEFAULT 0"
                    )
                )
                if not position_exists:
                    # Backfill existing rows with a stable per-(owner, project) order.
                    # Only runs the one time the column is first created.
                    conn.execute(
                        text(
                            """
                            UPDATE videos SET position = sub.rn - 1 FROM (
                                SELECT id, ROW_NUMBER() OVER (
                                    PARTITION BY owner_id, project_id ORDER BY created_at
                                ) AS rn
                                FROM videos
                            ) sub
                            WHERE videos.id = sub.id
                            """
                        )
                    )
            return
        except Exception as exc:
            if attempt == max_attempts:
                raise RuntimeError("Database is not ready after multiple retries") from exc
            logger.warning("Database not ready (attempt %d/%d): %s", attempt, max_attempts, exc)
            time.sleep(2)

FRONTEND_ORIGINS = [
    origin.strip()
    for origin in os.getenv("FRONTEND_ORIGINS", "http://localhost:4287").split(",")
    if origin.strip()
]

app.add_middleware(
    CORSMiddleware,
    allow_origins=FRONTEND_ORIGINS,
    allow_credentials=True,
    allow_methods=["*"],
    allow_headers=["*"],
)

VIDEOS_DIR = Path("videos")
TEXTS_DIR = Path("texts")
FRAMES_DIR = Path("frames")
VIDEOS_DIR.mkdir(exist_ok=True)
TEXTS_DIR.mkdir(exist_ok=True)
FRAMES_DIR.mkdir(exist_ok=True)

# Transcription and the optional LLM caption rewrite both run on OpenAI.
openai_api_key = os.getenv("OPENAI_API_KEY")

@app.get("/api/health")
def health_check(db: Session = Depends(get_db)):
    try:
        db.execute(text("SELECT 1"))
    except Exception:
        logger.exception("Health check DB ping failed")
        raise HTTPException(status_code=503, detail="Database unavailable")
    return {"status": "ok"}

# Pydantic models for request/response
class UserCreate(BaseModel):
    username: str
    password: str

class UserResponse(BaseModel):
    id: int
    username: str
    role: str
    created_at: datetime

    class Config:
        from_attributes = True

class Token(BaseModel):
    access_token: str
    token_type: str = "bearer"

class RoleUpdateRequest(BaseModel):
    role: str

class ResetKeyResponse(BaseModel):
    username: str
    reset_key: str
    expires_at: datetime

class ResetPasswordRequest(BaseModel):
    username: str
    reset_key: str
    new_password: str

class AdminStats(BaseModel):
    total_users: int
    total_videos: int
    transcribed_videos: int
    total_storage_bytes: int

class AdminVideoItem(BaseModel):
    id: int
    filename: str
    original_name: str
    owner_username: Optional[str] = None
    has_transcript: bool
    size_bytes: int
    created_at: datetime
    is_deleted: bool

class AdminOverviewResponse(BaseModel):
    stats: AdminStats
    videos: List[AdminVideoItem]

class FrameSelectionRequest(BaseModel):
    filenames: List[str]

class CaptionSegment(BaseModel):
    start: float
    end: float
    text: str

class VideoResponse(BaseModel):
    id: int
    filename: str
    original_name: str
    has_transcript: bool
    transcript_text: Optional[str] = None
    caption_segments: Optional[List[CaptionSegment]] = None
    # Free-form so a new field in pricing.py / caption_polish.py reaches the UI
    # without a matching schema change here.
    transcribe_stats: Optional[dict] = None
    polish_report: Optional[dict] = None
    rewrite_stats: Optional[dict] = None
    project_id: Optional[int] = None
    created_at: datetime

    class Config:
        from_attributes = True

class VideoUpdateRequest(BaseModel):
    original_name: str

class CaptionSegmentUpdateRequest(BaseModel):
    text: str

class CaptionReplaceRequest(BaseModel):
    """A find & replace over a video's caption lines."""

    find: str
    replace: str
    match_case: bool = False
    # None means every line; a list limits the run to those cue indexes, which is how
    # a single-line replace is expressed.
    segment_indexes: Optional[List[int]] = None
    # Also teach the pair to the word system, so the next transcript fixes it itself.
    save_to_glossary: bool = False

class CaptionReplacementItem(BaseModel):
    id: int
    find_text: str
    replace_text: str
    match_case: bool
    occurrences: int
    segments_changed: int
    segment_indexes: List[int] = []
    glossary_rule_id: Optional[str] = None
    created_at: datetime

    class Config:
        from_attributes = True

class CaptionReplaceResponse(BaseModel):
    video: VideoResponse
    occurrences: int
    segments_changed: int
    glossary_rule_id: Optional[str] = None
    history: CaptionReplacementItem

class TranscribeRequest(BaseModel):
    model: Optional[str] = None
    # The clean-up pass is deterministic and free, so it runs by default.
    polish: bool = True

class PolishRequest(BaseModel):
    split_lines: bool = True
    min_chars: int = caption_polish.MIN_LINE_CHARS
    max_chars: int = caption_polish.MAX_LINE_CHARS

class TranscribeModel(BaseModel):
    id: str
    label: str
    description: str
    provider: str
    is_default: bool
    estimated_cost_per_minute_usd: float
    estimated_seconds_per_minute: float

class TranscribeEstimate(BaseModel):
    model: str
    provider: str
    audio_seconds: float
    estimated_seconds: float
    estimated_cost_usd: float
    basis: str
    is_estimate_only: bool

class GlossaryRuleRequest(BaseModel):
    right: str
    wrong: List[str] = []
    category: str = "term"
    note: Optional[str] = None
    # A protected entry keeps a Thai transliteration as-is and shields it from every
    # other rule; it needs no `wrong` list.
    is_protected: bool = False

class GlossaryRuleToggleRequest(BaseModel):
    enabled: bool

class GlossaryRuleResponse(BaseModel):
    id: str
    right: str
    wrong: List[str]
    category: str
    category_label: str
    note: str
    source: str
    enabled: bool

class GlossaryResponse(BaseModel):
    rules: List[GlossaryRuleResponse]
    protected: List[str]
    categories: dict
    stats: dict

class GlossaryPreviewRequest(BaseModel):
    text: str

class GlossaryPreviewResponse(BaseModel):
    before: str
    after: str
    corrections: List[dict]

class VideoMoveRequest(BaseModel):
    project_id: Optional[int] = None

class ProjectCreate(BaseModel):
    name: str

class ProjectResponse(BaseModel):
    id: int
    name: str
    created_at: datetime
    video_count: int

class ReorderRequest(BaseModel):
    project_id: Optional[int] = None
    video_ids: List[int]

class FrameExtractionRequest(BaseModel):
    count: int

class FrameResponse(BaseModel):
    filename: str
    url: str

class FrameExtractionResponse(BaseModel):
    video_id: int
    count: int
    frames: List[FrameResponse]

# ---------------------------------------------------------------------------
# Auth & user management
# ---------------------------------------------------------------------------
@app.post("/api/auth/register", response_model=UserResponse, status_code=201)
def register(payload: UserCreate, db: Session = Depends(get_db)):
    username = payload.username.strip()
    if not username or not payload.password:
        raise HTTPException(status_code=400, detail="Username and password are required")
    if len(payload.password) < 6:
        raise HTTPException(status_code=400, detail="Password must be at least 6 characters")

    existing = db.query(models.User).filter(models.User.username == username).first()
    if existing:
        raise HTTPException(status_code=409, detail="Username already taken")

    # First registered user becomes the admin.
    is_first_user = db.query(models.User).count() == 0
    user = models.User(
        username=username,
        hashed_password=auth.hash_password(payload.password),
        role="admin" if is_first_user else "user",
    )
    db.add(user)
    db.commit()
    db.refresh(user)
    return user

@app.post("/api/auth/login", response_model=Token)
def login(form_data: OAuth2PasswordRequestForm = Depends(), db: Session = Depends(get_db)):
    user = db.query(models.User).filter(models.User.username == form_data.username).first()
    if not user or not auth.verify_password(form_data.password, user.hashed_password):
        raise HTTPException(status_code=401, detail="Incorrect username or password")
    return Token(access_token=auth.create_access_token(user.username))

@app.get("/api/auth/me", response_model=UserResponse)
def read_me(current_user: models.User = Depends(auth.get_current_user)):
    return current_user

@app.post("/api/auth/reset-password", status_code=204)
def reset_password(payload: ResetPasswordRequest, db: Session = Depends(get_db)):
    if len(payload.new_password) < 6:
        raise HTTPException(status_code=400, detail="Password must be at least 6 characters")

    invalid_exception = HTTPException(status_code=400, detail="Invalid or expired reset key")

    user = db.query(models.User).filter(models.User.username == payload.username.strip()).first()
    if (
        not user
        or not user.reset_token_hash
        or not user.reset_token_expires_at
        or user.reset_token_expires_at < datetime.now(timezone.utc)
        or not auth.verify_password(payload.reset_key, user.reset_token_hash)
    ):
        raise invalid_exception

    user.hashed_password = auth.hash_password(payload.new_password)
    user.reset_token_hash = None
    user.reset_token_expires_at = None
    db.commit()
    return Response(status_code=204)

@app.get("/api/users", response_model=List[UserResponse])
def list_users(_: models.User = Depends(auth.require_admin), db: Session = Depends(get_db)):
    return db.query(models.User).order_by(models.User.created_at.asc()).all()

@app.patch("/api/users/{user_id}", response_model=UserResponse)
def update_user_role(
    user_id: int,
    payload: RoleUpdateRequest,
    current_user: models.User = Depends(auth.require_admin),
    db: Session = Depends(get_db),
):
    if payload.role not in ("admin", "user"):
        raise HTTPException(status_code=400, detail="Role must be 'admin' or 'user'")

    user = db.query(models.User).filter(models.User.id == user_id).first()
    if not user:
        raise HTTPException(status_code=404, detail="User not found")
    if user.id == current_user.id and payload.role != "admin":
        raise HTTPException(status_code=400, detail="You cannot revoke your own admin role")

    user.role = payload.role
    db.commit()
    db.refresh(user)
    return user

@app.delete("/api/users/{user_id}", status_code=204)
def delete_user(
    user_id: int,
    current_user: models.User = Depends(auth.require_admin),
    db: Session = Depends(get_db),
):
    if user_id == current_user.id:
        raise HTTPException(status_code=400, detail="You cannot delete your own account")
    user = db.query(models.User).filter(models.User.id == user_id).first()
    if not user:
        raise HTTPException(status_code=404, detail="User not found")
    db.delete(user)
    db.commit()
    return Response(status_code=204)

@app.post("/api/users/{user_id}/reset-password", response_model=ResetKeyResponse)
def issue_reset_key(
    user_id: int,
    _: models.User = Depends(auth.require_admin),
    db: Session = Depends(get_db),
):
    user = db.query(models.User).filter(models.User.id == user_id).first()
    if not user:
        raise HTTPException(status_code=404, detail="User not found")

    token, token_hash, expires_at = auth.generate_reset_token()
    user.reset_token_hash = token_hash
    user.reset_token_expires_at = expires_at
    db.commit()

    return ResetKeyResponse(username=user.username, reset_key=token, expires_at=expires_at)

@app.get("/api/admin/overview", response_model=AdminOverviewResponse)
def admin_overview(
    _: models.User = Depends(auth.require_admin),
    db: Session = Depends(get_db),
):
    rows = (
        db.query(models.Video, models.User.username)
        .outerjoin(models.User, models.Video.owner_id == models.User.id)
        .order_by(models.Video.created_at.desc())
        .all()
    )

    videos: List[AdminVideoItem] = []
    total_storage_bytes = 0
    total_videos = 0
    transcribed_videos = 0
    for video, owner_username in rows:
        file_path = VIDEOS_DIR / video.filename
        size_bytes = file_path.stat().st_size if file_path.exists() else 0
        total_storage_bytes += size_bytes
        if not video.is_deleted:
            total_videos += 1
            if video.has_transcript:
                transcribed_videos += 1
        videos.append(
            AdminVideoItem(
                id=video.id,
                filename=video.filename,
                original_name=video.original_name,
                owner_username=owner_username,
                has_transcript=video.has_transcript,
                size_bytes=size_bytes,
                created_at=video.created_at,
                is_deleted=video.is_deleted,
            )
        )

    total_users = db.query(models.User).count()

    return AdminOverviewResponse(
        stats=AdminStats(
            total_users=total_users,
            total_videos=total_videos,
            transcribed_videos=transcribed_videos,
            total_storage_bytes=total_storage_bytes,
        ),
        videos=videos,
    )

def _get_owned_video(video_id: int, db: Session, current_user: models.User) -> models.Video:
    video = db.query(models.Video).filter(models.Video.id == video_id).first()
    if not video or video.owner_id != current_user.id or video.is_deleted:
        raise HTTPException(status_code=404, detail="Video not found")
    return video

def _next_position(db: Session, owner_id: int, project_id: Optional[int]) -> int:
    max_position = (
        db.query(func.max(models.Video.position))
        .filter(models.Video.owner_id == owner_id, models.Video.project_id == project_id)
        .scalar()
    )
    return (max_position + 1) if max_position is not None else 0

@app.post("/api/projects", response_model=ProjectResponse, status_code=201)
def create_project(
    payload: ProjectCreate,
    db: Session = Depends(get_db),
    current_user: models.User = Depends(auth.get_current_user),
):
    name = payload.name.strip()
    if not name:
        raise HTTPException(status_code=400, detail="Project name is required")

    project = models.Project(name=name, owner_id=current_user.id)
    db.add(project)
    db.commit()
    db.refresh(project)
    return ProjectResponse(id=project.id, name=project.name, created_at=project.created_at, video_count=0)

@app.get("/api/projects", response_model=List[ProjectResponse])
def list_projects(db: Session = Depends(get_db), current_user: models.User = Depends(auth.get_current_user)):
    projects = (
        db.query(models.Project)
        .filter(models.Project.owner_id == current_user.id)
        .order_by(models.Project.created_at.asc())
        .all()
    )
    counts = dict(
        db.query(models.Video.project_id, func.count(models.Video.id))
        .filter(
            models.Video.owner_id == current_user.id,
            models.Video.is_deleted.is_(False),
            models.Video.project_id.isnot(None),
        )
        .group_by(models.Video.project_id)
        .all()
    )
    return [
        ProjectResponse(id=p.id, name=p.name, created_at=p.created_at, video_count=counts.get(p.id, 0))
        for p in projects
    ]

def _reserve_video_path(original_filename: str) -> tuple[Path, str]:
    """Atomically claim a filename for a new upload under `VIDEOS_DIR`.

    Opens with O_CREAT | O_EXCL so two uploads racing on the same name (a real
    possibility now that uploads run concurrently) can never both win the same path —
    the loser retries with the next numbered suffix instead of silently overwriting or
    interleaving bytes into the winner's file. A plain `exists()` check-then-create, as
    this used to be, has exactly that race.
    """
    # basename() strips any directory component a crafted filename might carry
    # (e.g. "../../etc/passwd"), so the reserved path can never leave VIDEOS_DIR.
    safe_filename = os.path.basename(original_filename).replace(" ", "_") or "video"
    stem = Path(safe_filename).stem or "video"
    suffix = Path(safe_filename).suffix
    candidate = safe_filename
    counter = 1
    while True:
        path = VIDEOS_DIR / candidate
        try:
            fd = os.open(path, os.O_CREAT | os.O_EXCL | os.O_WRONLY)
            os.close(fd)
            return path, candidate
        except FileExistsError:
            candidate = f"{stem}_{counter}{suffix}"
            counter += 1

# Batched to this many bytes before each write, so the event loop is handed back to
# other requests (uploads included) after every small network read instead of once
# per whole file, while still avoiding a threadpool dispatch per tiny TCP chunk.
UPLOAD_WRITE_CHUNK_BYTES = 4 * 1024 * 1024

@app.post("/api/videos", response_model=VideoResponse, status_code=201)
async def upload_video(
    request: Request,
    filename: str = Query(..., min_length=1, description="Original filename from the client"),
    project_id: Optional[int] = Query(None),
    db: Session = Depends(get_db),
    current_user: models.User = Depends(auth.get_current_user),
):
    """Stream the request body straight to disk.

    Deliberately not a `File(...)`/multipart endpoint: FastAPI's multipart parser
    spools the upload into a temp file first and our old code then copied *that* into
    `videos/` — every upload hit disk twice, and the copy was a blocking synchronous
    call inside an async endpoint, which froze the whole worker (every other request
    on it, including other uploads) until it finished. Reading the raw body directly
    means one disk write, and awaiting the stream yields control between chunks so
    multiple uploads — and everything else — actually run concurrently.
    """
    if project_id is not None:
        project = (
            db.query(models.Project)
            .filter(models.Project.id == project_id, models.Project.owner_id == current_user.id)
            .first()
        )
        if not project:
            raise HTTPException(status_code=404, detail="Project not found")

    file_path, safe_filename = _reserve_video_path(filename)

    try:
        pending: list[bytes] = []
        pending_size = 0
        with open(file_path, "wb") as buffer:
            async for chunk in request.stream():
                if not chunk:
                    continue
                pending.append(chunk)
                pending_size += len(chunk)
                if pending_size >= UPLOAD_WRITE_CHUNK_BYTES:
                    await run_in_threadpool(buffer.write, b"".join(pending))
                    pending.clear()
                    pending_size = 0
            if pending:
                await run_in_threadpool(buffer.write, b"".join(pending))
    except Exception:
        file_path.unlink(missing_ok=True)
        raise

    if file_path.stat().st_size == 0:
        file_path.unlink(missing_ok=True)
        raise HTTPException(status_code=400, detail="Uploaded file is empty")

    db_video = models.Video(
        filename=safe_filename,
        original_name=os.path.basename(filename) or filename,
        owner_id=current_user.id,
        project_id=project_id,
        position=_next_position(db, current_user.id, project_id),
    )
    db.add(db_video)
    db.commit()
    db.refresh(db_video)

    return db_video

@app.get("/api/videos", response_model=List[VideoResponse])
def list_videos(
    project_id: Optional[int] = None,
    ungrouped: bool = False,
    db: Session = Depends(get_db),
    current_user: models.User = Depends(auth.get_current_user),
):
    query = db.query(models.Video).filter(
        models.Video.owner_id == current_user.id, models.Video.is_deleted.is_(False)
    )
    if ungrouped:
        return query.filter(models.Video.project_id.is_(None)).order_by(models.Video.position.asc()).all()
    if project_id is not None:
        return query.filter(models.Video.project_id == project_id).order_by(models.Video.position.asc()).all()
    return query.order_by(models.Video.created_at.desc()).all()

@app.post("/api/videos/reorder", status_code=204)
def reorder_videos(
    payload: ReorderRequest,
    db: Session = Depends(get_db),
    current_user: models.User = Depends(auth.get_current_user),
):
    videos = (
        db.query(models.Video)
        .filter(
            models.Video.owner_id == current_user.id,
            models.Video.is_deleted.is_(False),
            models.Video.project_id == payload.project_id,
        )
        .all()
    )
    video_map = {v.id: v for v in videos}
    if set(payload.video_ids) != set(video_map.keys()):
        raise HTTPException(status_code=400, detail="video_ids must match every video in this view")

    for index, video_id in enumerate(payload.video_ids):
        video_map[video_id].position = index
    db.commit()
    return Response(status_code=204)

@app.get("/api/videos/{video_id}", response_model=VideoResponse)
def get_video(video_id: int, db: Session = Depends(get_db), current_user: models.User = Depends(auth.get_current_user)):
    return _get_owned_video(video_id, db, current_user)

@app.delete("/api/videos/{video_id}", status_code=204)
def delete_video(
    video_id: int,
    db: Session = Depends(get_db),
    current_user: models.User = Depends(auth.get_current_user),
):
    video = _get_owned_video(video_id, db, current_user)
    video.is_deleted = True
    video.deleted_at = datetime.now(timezone.utc)
    db.commit()
    return Response(status_code=204)

@app.patch("/api/videos/{video_id}", response_model=VideoResponse)
def rename_video(
    video_id: int,
    payload: VideoUpdateRequest,
    db: Session = Depends(get_db),
    current_user: models.User = Depends(auth.get_current_user),
):
    new_name = payload.original_name.strip()
    if not new_name:
        raise HTTPException(status_code=400, detail="Name must not be empty")

    video = _get_owned_video(video_id, db, current_user)
    video.original_name = new_name
    db.commit()
    db.refresh(video)
    return video

@app.patch("/api/videos/{video_id}/move", response_model=VideoResponse)
def move_video(
    video_id: int,
    payload: VideoMoveRequest,
    db: Session = Depends(get_db),
    current_user: models.User = Depends(auth.get_current_user),
):
    video = _get_owned_video(video_id, db, current_user)

    if payload.project_id is not None:
        project = (
            db.query(models.Project)
            .filter(models.Project.id == payload.project_id, models.Project.owner_id == current_user.id)
            .first()
        )
        if not project:
            raise HTTPException(status_code=404, detail="Project not found")

    video.project_id = payload.project_id
    video.position = _next_position(db, current_user.id, payload.project_id)
    db.commit()
    db.refresh(video)
    return video

@app.get("/api/videos/stream/{filename}")
def stream_video(
    filename: str,
    db: Session = Depends(get_db),
    current_user: models.User = Depends(auth.get_current_user_for_media),
):
    video = db.query(models.Video).filter(models.Video.filename == filename).first()
    if not video or video.owner_id != current_user.id or video.is_deleted:
        raise HTTPException(status_code=404, detail="Video not found")
    file_path = VIDEOS_DIR / filename
    if not file_path.exists() or not file_path.is_file():
        raise HTTPException(status_code=404, detail="Video not found")
    return FileResponse(file_path)

@app.post("/api/videos/{video_id}/frames", response_model=FrameExtractionResponse)
def extract_video_frames(
    video_id: int,
    payload: FrameExtractionRequest,
    db: Session = Depends(get_db),
    current_user: models.User = Depends(auth.get_current_user),
):
    if payload.count < 1:
        raise HTTPException(status_code=400, detail="Frame count must be at least 1")

    video = _get_owned_video(video_id, db, current_user)

    file_path = VIDEOS_DIR / video.filename
    if not file_path.exists():
        raise HTTPException(status_code=404, detail="Video file not found on disk")

    frame_dir = FRAMES_DIR / f"video_{video_id}"
    if frame_dir.exists():
        shutil.rmtree(frame_dir)
    frame_dir.mkdir(parents=True, exist_ok=True)

    try:
        frame_paths = pipeline.extract_evenly_spaced_frames(str(file_path), payload.count, str(frame_dir))
    except Exception:
        logger.exception("Frame extraction failed for video %s", video_id)
        raise HTTPException(status_code=500, detail="Frame extraction failed")

    return FrameExtractionResponse(
        video_id=video_id,
        count=payload.count,
        frames=[
            FrameResponse(
                filename=frame_path.name,
                url=f"/api/videos/{video_id}/frames/{frame_path.name}",
            )
            for frame_path in frame_paths
        ],
    )

@app.get("/api/videos/{video_id}/frames", response_model=FrameExtractionResponse)
def list_video_frames(video_id: int, db: Session = Depends(get_db), current_user: models.User = Depends(auth.get_current_user)):
    _get_owned_video(video_id, db, current_user)

    frame_dir = FRAMES_DIR / f"video_{video_id}"
    if not frame_dir.exists():
        return FrameExtractionResponse(video_id=video_id, count=0, frames=[])

    frame_paths = sorted(frame_dir.glob("*.jpg")) + sorted(frame_dir.glob("*.png"))
    frames = [
        FrameResponse(
            filename=p.name,
            url=f"/api/videos/{video_id}/frames/{p.name}",
        )
        for p in frame_paths
    ]
    return FrameExtractionResponse(video_id=video_id, count=len(frames), frames=frames)

def _build_frames_zip(video_id: int, filenames: Optional[List[str]] = None) -> io.BytesIO:
    frame_dir = FRAMES_DIR / f"video_{video_id}"
    if not frame_dir.exists():
        raise HTTPException(status_code=404, detail="No frames available")

    available = sorted(frame_dir.glob("*.jpg")) + sorted(frame_dir.glob("*.png"))
    if filenames is not None:
        wanted = set(filenames)
        selected = [p for p in available if p.name in wanted]
    else:
        selected = available

    if not selected:
        raise HTTPException(status_code=404, detail="No matching frames found")

    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", zipfile.ZIP_DEFLATED) as archive:
        for path in selected:
            archive.write(path, arcname=path.name)
    buffer.seek(0)
    return buffer

@app.get("/api/videos/{video_id}/frames/download")
def download_all_frames(
    video_id: int,
    db: Session = Depends(get_db),
    current_user: models.User = Depends(auth.get_current_user),
):
    _get_owned_video(video_id, db, current_user)
    buffer = _build_frames_zip(video_id)
    return StreamingResponse(
        buffer,
        media_type="application/zip",
        headers={"Content-Disposition": f'attachment; filename="video_{video_id}_frames.zip"'},
    )

@app.post("/api/videos/{video_id}/frames/download")
def download_selected_frames(
    video_id: int,
    payload: FrameSelectionRequest,
    db: Session = Depends(get_db),
    current_user: models.User = Depends(auth.get_current_user),
):
    _get_owned_video(video_id, db, current_user)
    safe_names = [Path(name).name for name in payload.filenames]
    buffer = _build_frames_zip(video_id, safe_names)
    return StreamingResponse(
        buffer,
        media_type="application/zip",
        headers={"Content-Disposition": f'attachment; filename="video_{video_id}_frames.zip"'},
    )

@app.get("/api/videos/{video_id}/frames/{frame_filename}")
def get_video_frame(
    video_id: int,
    frame_filename: str,
    db: Session = Depends(get_db),
    current_user: models.User = Depends(auth.get_current_user_for_media),
):
    if Path(frame_filename).name != frame_filename:
        raise HTTPException(status_code=400, detail="Invalid frame filename")

    _get_owned_video(video_id, db, current_user)

    frame_path = FRAMES_DIR / f"video_{video_id}" / frame_filename
    if not frame_path.exists() or not frame_path.is_file():
        raise HTTPException(status_code=404, detail="Frame not found")
    return FileResponse(frame_path)

@app.get("/api/videos/{video_id}/captions.vtt")
def get_video_captions_vtt(
    video_id: int,
    db: Session = Depends(get_db),
    current_user: models.User = Depends(auth.get_current_user_for_media),
):
    video = _get_owned_video(video_id, db, current_user)
    if not video.caption_segments:
        raise HTTPException(status_code=404, detail="Captions not available")
    return Response(content=pipeline.segments_to_vtt(video.caption_segments), media_type="text/vtt")

@app.get("/api/videos/{video_id}/captions.srt")
def get_video_captions_srt(
    video_id: int,
    db: Session = Depends(get_db),
    current_user: models.User = Depends(auth.get_current_user),
):
    video = _get_owned_video(video_id, db, current_user)
    if not video.caption_segments:
        raise HTTPException(status_code=404, detail="Captions not available")
    return Response(content=pipeline.segments_to_srt(video.caption_segments), media_type="application/x-subrip")

@app.post("/api/videos/{video_id}/captions", response_model=VideoResponse)
async def upload_captions(
    video_id: int,
    file: UploadFile = File(...),
    db: Session = Depends(get_db),
    current_user: models.User = Depends(auth.get_current_user),
):
    video = _get_owned_video(video_id, db, current_user)

    raw = await file.read()
    try:
        text = raw.decode("utf-8-sig")
    except UnicodeDecodeError:
        try:
            text = raw.decode("cp1252")
        except UnicodeDecodeError:
            raise HTTPException(status_code=400, detail="Could not decode the file as text")

    try:
        segments = pipeline.parse_srt(text)
    except ValueError as exc:
        raise HTTPException(status_code=400, detail=str(exc))

    video.caption_segments = segments
    video.transcript_text = " ".join(seg["text"] for seg in segments)
    video.has_transcript = True
    db.commit()
    db.refresh(video)
    return video

@app.patch("/api/videos/{video_id}/captions/{segment_index}", response_model=VideoResponse)
def update_caption_segment(
    video_id: int,
    segment_index: int,
    payload: CaptionSegmentUpdateRequest,
    db: Session = Depends(get_db),
    current_user: models.User = Depends(auth.get_current_user),
):
    video = _get_owned_video(video_id, db, current_user)

    segments = video.caption_segments or []
    if not 0 <= segment_index < len(segments):
        raise HTTPException(status_code=404, detail="Caption segment not found")

    new_text = payload.text.strip()
    if not new_text:
        raise HTTPException(status_code=400, detail="Caption text must not be empty")

    # JSON columns are only flushed when reassigned, so rebuild the list.
    updated = [dict(seg) for seg in segments]
    updated[segment_index]["text"] = new_text

    video.caption_segments = updated
    video.transcript_text = " ".join(seg["text"] for seg in updated)
    db.commit()
    db.refresh(video)
    return video

def _compile_caption_find(find: str, match_case: bool) -> "re.Pattern[str]":
    """Literal search pattern. Escaped on purpose: the box is find & replace, not regex."""
    return re.compile(re.escape(find), 0 if match_case else re.IGNORECASE)

@app.get(
    "/api/videos/{video_id}/captions/replacements",
    response_model=List[CaptionReplacementItem],
)
def list_caption_replacements(
    video_id: int,
    db: Session = Depends(get_db),
    current_user: models.User = Depends(auth.get_current_user),
):
    """Every find & replace run on this video, newest first."""
    _get_owned_video(video_id, db, current_user)
    rows = (
        db.query(models.CaptionReplacement)
        .filter(models.CaptionReplacement.video_id == video_id)
        .order_by(models.CaptionReplacement.id.desc())
        .all()
    )
    return rows

@app.post("/api/videos/{video_id}/captions/replace", response_model=CaptionReplaceResponse)
def replace_in_captions(
    video_id: int,
    payload: CaptionReplaceRequest,
    db: Session = Depends(get_db),
    current_user: models.User = Depends(auth.get_current_user),
):
    """Replace a phrase across the caption lines and record what was changed.

    Every run is logged in `caption_replacements` (the before and after text), and can
    optionally be taught to the word system so the next transcript fixes it on its own.
    """
    video = _get_owned_video(video_id, db, current_user)

    find = payload.find
    if not find.strip():
        raise HTTPException(status_code=400, detail="Enter the text to find")
    replace = payload.replace
    if find == replace:
        raise HTTPException(status_code=400, detail="The replacement is the same as the text found")

    segments = video.caption_segments or []
    if not segments:
        raise HTTPException(status_code=404, detail="Captions not available")

    if payload.segment_indexes is None:
        targets = range(len(segments))
    else:
        out_of_range = [i for i in payload.segment_indexes if not 0 <= i < len(segments)]
        if out_of_range:
            raise HTTPException(status_code=404, detail="Caption segment not found")
        targets = sorted(set(payload.segment_indexes))

    pattern = _compile_caption_find(find, payload.match_case)
    is_deletion = not replace.strip()

    # JSON columns are only flushed when reassigned, so rebuild the list.
    updated = [dict(seg) for seg in segments]
    occurrences = 0
    changed_indexes: List[int] = []

    for index in targets:
        original = updated[index].get("text") or ""
        # A plain lambda, so a backslash group reference in the replacement stays literal.
        new_text, count = pattern.subn(lambda _match: replace, original)
        if not count:
            continue
        if is_deletion:
            # Deleting a word leaves the spaces that surrounded it behind.
            new_text = re.sub(r"[ \t]{2,}", " ", new_text)
        new_text = new_text.strip()
        if not new_text:
            raise HTTPException(
                status_code=400,
                detail=f"Line {index + 1} would be left empty by this replacement",
            )
        updated[index]["text"] = new_text
        occurrences += count
        changed_indexes.append(index)

    if not occurrences:
        raise HTTPException(status_code=404, detail=f"No caption line contains {find!r}")

    rule_id: Optional[str] = None
    if payload.save_to_glossary:
        rule_id = _teach_glossary_pair(db, current_user, wrong=find, right=replace, video=video)

    history = models.CaptionReplacement(
        video_id=video.id,
        owner_id=current_user.id,
        find_text=find,
        replace_text=replace,
        match_case=payload.match_case,
        occurrences=occurrences,
        segments_changed=len(changed_indexes),
        segment_indexes=changed_indexes,
        glossary_rule_id=rule_id,
    )
    db.add(history)

    video.caption_segments = updated
    video.transcript_text = " ".join(seg["text"] for seg in updated)
    db.commit()
    db.refresh(video)
    db.refresh(history)

    return {
        "video": video,
        "occurrences": occurrences,
        "segments_changed": len(changed_indexes),
        "glossary_rule_id": rule_id,
        "history": history,
    }

# ---------------------------------------------------------------------------
# The word system (ASR correction glossary)
# ---------------------------------------------------------------------------
def _glossary_rows(db: Session, user: models.User) -> List[models.GlossaryRule]:
    """A user's own glossary rows plus any shared ones with no owner.

    Shared rows come first so that when both exist for the same rule id, the user's
    own row is applied last and wins. Sorted in Python because NULLS FIRST ordering
    is not portable across our SQLite and Postgres targets.
    """
    rows = (
        db.query(models.GlossaryRule)
        .filter(
            (models.GlossaryRule.owner_id == user.id)
            | (models.GlossaryRule.owner_id.is_(None))
        )
        .all()
    )
    return sorted(rows, key=lambda row: (row.rule_id, row.owner_id is not None))

def _build_glossary(db: Session, user: models.User) -> glossary_module.Glossary:
    """Compile the built-in word system with this user's additions layered on top."""
    rows = _glossary_rows(db, user)
    extra_rules = [
        {
            "id": row.rule_id,
            "right": row.right,
            "wrong": list(row.wrong or []),
            "category": row.category,
            "note": row.note or "",
            "source": "user",
        }
        for row in rows
        if not row.is_protected
    ]
    extra_protected = [row.right for row in rows if row.is_protected and not row.is_disabled]
    disabled = {row.rule_id for row in rows if row.is_disabled}
    return glossary_module.build_glossary(
        extra_rules=extra_rules,
        disabled_rule_ids=disabled,
        extra_protected=extra_protected,
    )

# A rule is meant to be a term, not a sentence; longer finds are one-off edits.
MAX_GLOSSARY_TERM_CHARS = 80

def _teach_glossary_pair(
    db: Session,
    user: models.User,
    *,
    wrong: str,
    right: str,
    video: models.Video,
) -> str:
    """Record a `wrong -> right` pair from a caption edit and return its rule id.

    Unlike `upsert_glossary_rule` this never clears an existing `wrong` list: a find &
    replace teaches one more misheard spelling, it does not redefine the rule. If a rule
    for that correct spelling already exists it is extended in place, whatever category
    it sits in, so "Roboflow" does not end up as both a built-in platform rule and a
    near-identical term rule. An existing rule is also copied into the user's own row
    first, because a user row replaces a rule wholesale when the glossary is compiled.
    """
    wrong = wrong.strip()
    right = right.strip()
    if not wrong or not right:
        raise HTTPException(
            status_code=400,
            detail="A word-system entry needs both the wrong and the correct spelling",
        )
    if len(wrong) > MAX_GLOSSARY_TERM_CHARS or len(right) > MAX_GLOSSARY_TERM_CHARS:
        raise HTTPException(
            status_code=400,
            detail=(
                "This phrase is too long for the word system "
                f"(limit {MAX_GLOSSARY_TERM_CHARS} characters) — replace it without saving"
            ),
        )

    existing = next(
        (
            rule
            for rule in _build_glossary(db, user).rules
            if rule.right.lower() == right.lower()
        ),
        None,
    )
    rule_id = (
        existing.id
        if existing
        else glossary_module.Rule(right=right, wrong=[wrong], category="term").id
    )

    row = (
        db.query(models.GlossaryRule)
        .filter(
            models.GlossaryRule.rule_id == rule_id,
            models.GlossaryRule.owner_id == user.id,
        )
        .first()
    )
    if row is None:
        row = models.GlossaryRule(
            owner_id=user.id,
            rule_id=rule_id,
            right=existing.right if existing else right,
            wrong=list(existing.wrong) if existing else [],
            category=existing.category if existing else "term",
            note=existing.note if existing else "",
        )
        db.add(row)

    variants = list(row.wrong or [])
    # The canonical spelling is already matched in any casing, so a pair that only
    # changes case ("roboflow" -> "Roboflow") adds no variant, just the rule.
    already_known = wrong.lower() == (row.right or "").lower() or any(
        variant.lower() == wrong.lower() for variant in variants
    )
    if not already_known:
        variants.append(wrong)
    row.wrong = variants
    row.is_protected = False
    row.is_disabled = False
    if not (row.note or "").strip():
        row.note = f"จากการแก้คำในคำบรรยายของ {video.original_name}"
    return rule_id

@app.get("/api/glossary", response_model=GlossaryResponse)
def get_glossary(db: Session = Depends(get_db), current_user: models.User = Depends(auth.get_current_user)):
    compiled = _build_glossary(db, current_user)
    return {
        "rules": compiled.rule_dicts(),
        "protected": compiled.protected,
        "categories": glossary_module.CATEGORY_LABELS,
        "stats": compiled.stats(),
    }

@app.post("/api/glossary", response_model=GlossaryRuleResponse, status_code=201)
def upsert_glossary_rule(
    payload: GlossaryRuleRequest,
    db: Session = Depends(get_db),
    current_user: models.User = Depends(auth.get_current_user),
):
    right = payload.right.strip()
    if not right:
        raise HTTPException(status_code=400, detail="'right' (the correct spelling) is required")

    wrong = [item.strip() for item in payload.wrong if item.strip()]
    if not wrong and not payload.is_protected:
        raise HTTPException(
            status_code=400,
            detail="Provide at least one misheard spelling, or mark the entry as protected",
        )

    category = "protected" if payload.is_protected else (payload.category or "term").strip()
    if category not in glossary_module.CATEGORY_LABELS:
        raise HTTPException(status_code=400, detail=f"Unknown category: {category}")

    # Same id format as the built-ins, so re-adding a built-in's spelling overrides it
    # rather than creating a near-duplicate rule.
    rule_id = glossary_module.Rule(right=right, wrong=wrong, category=category).id

    # Scoped to this user on purpose: saving over a shared (owner-less) rule would
    # change it for everyone, so we create a personal override instead.
    row = (
        db.query(models.GlossaryRule)
        .filter(
            models.GlossaryRule.rule_id == rule_id,
            models.GlossaryRule.owner_id == current_user.id,
        )
        .first()
    )
    if row is None:
        row = models.GlossaryRule(owner_id=current_user.id, rule_id=rule_id)
        db.add(row)

    row.right = right
    row.wrong = wrong
    row.category = category
    row.note = payload.note or ""
    row.is_protected = payload.is_protected
    row.is_disabled = False
    db.commit()
    db.refresh(row)

    return {
        "id": row.rule_id,
        "right": row.right,
        "wrong": list(row.wrong or []),
        "category": row.category,
        "category_label": glossary_module.CATEGORY_LABELS.get(row.category, row.category),
        "note": row.note or "",
        "source": "user",
        "enabled": not row.is_disabled,
    }

@app.patch("/api/glossary/{rule_id}", status_code=204)
def toggle_glossary_rule(
    rule_id: str,
    payload: GlossaryRuleToggleRequest,
    db: Session = Depends(get_db),
    current_user: models.User = Depends(auth.get_current_user),
):
    """Enable or disable a rule. Works for built-ins too, by storing a marker row."""
    row = (
        db.query(models.GlossaryRule)
        .filter(
            models.GlossaryRule.rule_id == rule_id,
            models.GlossaryRule.owner_id == current_user.id,
        )
        .first()
    )

    if row is None:
        builtin = next(
            (
                rule
                for rule in glossary_module.build_glossary().rules
                if rule.id == rule_id
            ),
            None,
        )
        if builtin is None:
            raise HTTPException(status_code=404, detail="Glossary rule not found")
        # Disabling a built-in is recorded as a user row carrying the same id.
        row = models.GlossaryRule(
            owner_id=current_user.id,
            rule_id=builtin.id,
            right=builtin.right,
            wrong=list(builtin.wrong),
            category=builtin.category,
            note=builtin.note,
        )
        db.add(row)

    row.is_disabled = not payload.enabled
    db.commit()
    return Response(status_code=204)

@app.delete("/api/glossary/{rule_id}", status_code=204)
def delete_glossary_rule(
    rule_id: str,
    db: Session = Depends(get_db),
    current_user: models.User = Depends(auth.get_current_user),
):
    row = (
        db.query(models.GlossaryRule)
        .filter(
            models.GlossaryRule.rule_id == rule_id,
            models.GlossaryRule.owner_id == current_user.id,
        )
        .first()
    )
    if row is None:
        raise HTTPException(status_code=404, detail="Glossary rule not found")
    db.delete(row)
    db.commit()
    return Response(status_code=204)

@app.post("/api/glossary/preview", response_model=GlossaryPreviewResponse)
def preview_glossary(
    payload: GlossaryPreviewRequest,
    db: Session = Depends(get_db),
    current_user: models.User = Depends(auth.get_current_user),
):
    """Run the word system over a sample line — for checking a rule before relying on it."""
    compiled = _build_glossary(db, current_user)
    corrected, corrections = compiled.correct(payload.text)
    return {
        "before": payload.text,
        "after": caption_polish.normalize_spacing(corrected),
        "corrections": corrections,
    }

@app.get("/api/transcribe-models", response_model=List[TranscribeModel])
def list_transcribe_models(current_user: models.User = Depends(auth.get_current_user)):
    # Without a key, picking any model would only fail at transcribe time.
    if not openai_api_key:
        return []
    return pipeline.available_transcribe_models()

@app.post("/api/videos/{video_id}/transcribe", response_model=VideoResponse)
def transcribe_video(
    video_id: int,
    payload: Optional[TranscribeRequest] = None,
    db: Session = Depends(get_db),
    current_user: models.User = Depends(auth.get_current_user),
):
    try:
        model = pipeline.resolve_transcribe_model(payload.model if payload else None)
    except ValueError as exc:
        raise HTTPException(status_code=400, detail=str(exc))

    if not openai_api_key:
        raise HTTPException(
            status_code=500, detail=f"OPENAI_API_KEY is not set (required by {model})"
        )

    video = _get_owned_video(video_id, db, current_user)

    file_path = VIDEOS_DIR / video.filename
    if not file_path.exists():
        raise HTTPException(status_code=404, detail="Video file not found on disk")

    try:
        result = pipeline.process_video(
            str(file_path),
            openai_api_key,
            str(TEXTS_DIR),
            model=model,
            polish=payload.polish if payload else True,
            glossary=_build_glossary(db, current_user),
        )

        video.has_transcript = True
        video.transcript_text = result["text"]
        video.caption_segments = result["segments"]
        video.transcribe_stats = result["stats"]
        video.polish_report = result["polish_report"]
        db.commit()
        db.refresh(video)

        stats = result["stats"]
        logger.info(
            "Transcribed video %s with %s in %.1fs (%.0fs audio), est. $%.4f [%s]",
            video_id, model, stats["elapsed_seconds"], stats["audio_seconds"],
            stats["estimated_cost_usd"], stats["cost_basis"],
        )
        return video
    except Exception:
        logger.exception("Transcription failed for video %s (model=%s)", video_id, model)
        raise HTTPException(status_code=500, detail=f"Transcription failed using {model}")

@app.get("/api/videos/{video_id}/transcribe-estimate", response_model=TranscribeEstimate)
def estimate_transcription(
    video_id: int,
    model: Optional[str] = None,
    db: Session = Depends(get_db),
    current_user: models.User = Depends(auth.get_current_user),
):
    """Projected run time and cost, so the price is visible before the click."""
    try:
        resolved = pipeline.resolve_transcribe_model(model)
    except ValueError as exc:
        raise HTTPException(status_code=400, detail=str(exc))

    video = _get_owned_video(video_id, db, current_user)
    file_path = VIDEOS_DIR / video.filename
    if not file_path.exists():
        raise HTTPException(status_code=404, detail="Video file not found on disk")

    return pipeline.estimate_video_run(str(file_path), resolved)

@app.post("/api/videos/{video_id}/polish-captions", response_model=VideoResponse)
def polish_captions(
    video_id: int,
    payload: Optional[PolishRequest] = None,
    db: Session = Depends(get_db),
    current_user: models.User = Depends(auth.get_current_user),
):
    """Re-run the deterministic clean-up: word system, spacing, line split, timings.

    Costs nothing and needs no API key — unlike /rewrite-captions, which pays an LLM.
    Safe to re-run: the word system is idempotent, and a line already inside the
    character budget is left alone.
    """
    video = _get_owned_video(video_id, db, current_user)
    if not video.caption_segments:
        raise HTTPException(status_code=400, detail="No captions to polish. Transcribe first.")

    options = payload or PolishRequest()
    if options.min_chars < 1 or options.max_chars < options.min_chars:
        raise HTTPException(status_code=400, detail="Require 1 <= min_chars <= max_chars")

    try:
        segments, report = caption_polish.polish_segments(
            video.caption_segments,
            glossary=_build_glossary(db, current_user),
            min_chars=options.min_chars,
            max_chars=options.max_chars,
            split_lines=options.split_lines,
        )
        video.caption_segments = segments
        video.transcript_text = " ".join(segment["text"] for segment in segments)
        video.polish_report = report
        db.commit()
        db.refresh(video)
        return video
    except Exception:
        logger.exception("Caption polish failed for video %s", video_id)
        raise HTTPException(status_code=500, detail="Caption polish failed")

@app.post("/api/videos/{video_id}/rewrite-captions", response_model=VideoResponse)
def rewrite_captions(video_id: int, db: Session = Depends(get_db), current_user: models.User = Depends(auth.get_current_user)):
    if not openai_api_key:
        raise HTTPException(status_code=500, detail="OPENAI_API_KEY is not set")

    video = _get_owned_video(video_id, db, current_user)
    if not video.caption_segments:
        raise HTTPException(status_code=400, detail="No captions to rewrite. Transcribe first.")

    try:
        rewritten, rewrite_stats = pipeline.rewrite_caption_segments(
            video.caption_segments, openai_api_key
        )
        # The LLM rewrites text freely, which can re-introduce glued Thai/English and
        # push a line past the budget, so the deterministic pass runs after it.
        segments, report = caption_polish.polish_segments(
            rewritten, glossary=_build_glossary(db, current_user)
        )
        video.caption_segments = segments
        video.transcript_text = " ".join(seg["text"] for seg in segments)
        video.polish_report = report
        video.rewrite_stats = rewrite_stats
        db.commit()
        db.refresh(video)
        logger.info(
            "Rewrote captions for video %s with %s in %.1fs, est. $%.4f",
            video_id, rewrite_stats["model"], rewrite_stats["elapsed_seconds"],
            rewrite_stats["estimated_cost_usd"],
        )
        return video
    except Exception:
        logger.exception("Caption rewrite failed for video %s", video_id)
        raise HTTPException(status_code=500, detail="Caption rewrite failed")

# uvicorn api:app --reload --port 8000
