import logging
import os
import shutil
import time
import io
import zipfile
from datetime import datetime, timezone
from pathlib import Path
from fastapi import FastAPI, HTTPException, Depends, Form, UploadFile, File
from fastapi.responses import FileResponse, Response, StreamingResponse
from fastapi.middleware.cors import CORSMiddleware
from fastapi.security import OAuth2PasswordRequestForm
from sqlalchemy import func, text
from sqlalchemy.orm import Session
from dotenv import load_dotenv
from typing import List, Optional
from pydantic import BaseModel

import pipeline
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

# Transcription runs on Gemini or OpenAI (per-request model choice); caption rewriting
# runs on OpenAI only.
gemini_api_key = os.getenv("GEMINI_API_KEY")
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
    project_id: Optional[int] = None
    created_at: datetime

    class Config:
        from_attributes = True

class VideoUpdateRequest(BaseModel):
    original_name: str

class CaptionSegmentUpdateRequest(BaseModel):
    text: str

class TranscribeRequest(BaseModel):
    model: Optional[str] = None

class TranscribeModel(BaseModel):
    id: str
    label: str
    description: str
    provider: str
    is_default: bool

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

@app.post("/api/videos", response_model=VideoResponse, status_code=201)
async def upload_video(
    file: UploadFile = File(...),
    project_id: Optional[int] = Form(None),
    db: Session = Depends(get_db),
    current_user: models.User = Depends(auth.get_current_user),
):
    if project_id is not None:
        project = (
            db.query(models.Project)
            .filter(models.Project.id == project_id, models.Project.owner_id == current_user.id)
            .first()
        )
        if not project:
            raise HTTPException(status_code=404, detail="Project not found")

    safe_filename = file.filename.replace(" ", "_")
    file_path = VIDEOS_DIR / safe_filename

    # Ensure unique filename
    counter = 1
    stem = file_path.stem
    suffix = file_path.suffix
    while file_path.exists():
        safe_filename = f"{stem}_{counter}{suffix}"
        file_path = VIDEOS_DIR / safe_filename
        counter += 1

    with open(file_path, "wb") as buffer:
        shutil.copyfileobj(file.file, buffer)

    db_video = models.Video(
        filename=safe_filename,
        original_name=file.filename,
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

def _configured_providers() -> set:
    configured = set()
    if gemini_api_key:
        configured.add("gemini")
    if openai_api_key:
        configured.add("openai")
    return configured

@app.get("/api/transcribe-models", response_model=List[TranscribeModel])
def list_transcribe_models(current_user: models.User = Depends(auth.get_current_user)):
    # Hide models we have no key for — picking one would only fail at transcribe time.
    configured = _configured_providers()
    return [
        entry
        for entry in pipeline.available_transcribe_models()
        if entry["provider"] in configured
    ]

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

    provider = pipeline.transcribe_model_provider(model)
    if provider not in _configured_providers():
        key_name = "OPENAI_API_KEY" if provider == "openai" else "GEMINI_API_KEY"
        raise HTTPException(status_code=500, detail=f"{key_name} is not set (required by {model})")

    video = _get_owned_video(video_id, db, current_user)

    file_path = VIDEOS_DIR / video.filename
    if not file_path.exists():
        raise HTTPException(status_code=404, detail="Video file not found on disk")

    try:
        result = pipeline.process_video(
            str(file_path),
            gemini_api_key,
            str(TEXTS_DIR),
            model=model,
            openai_api_key=openai_api_key,
        )

        video.has_transcript = True
        video.transcript_text = result["text"]
        video.caption_segments = result["segments"]
        db.commit()
        db.refresh(video)

        return video
    except Exception:
        logger.exception("Transcription failed for video %s (model=%s)", video_id, model)
        raise HTTPException(status_code=500, detail=f"Transcription failed using {model}")

@app.post("/api/videos/{video_id}/rewrite-captions", response_model=VideoResponse)
def rewrite_captions(video_id: int, db: Session = Depends(get_db), current_user: models.User = Depends(auth.get_current_user)):
    if not openai_api_key:
        raise HTTPException(status_code=500, detail="OPENAI_API_KEY is not set")

    video = _get_owned_video(video_id, db, current_user)
    if not video.caption_segments:
        raise HTTPException(status_code=400, detail="No captions to rewrite. Transcribe first.")

    try:
        rewritten = pipeline.rewrite_caption_segments(video.caption_segments, openai_api_key)
        video.caption_segments = rewritten
        video.transcript_text = " ".join(seg["text"] for seg in rewritten)
        db.commit()
        db.refresh(video)
        return video
    except Exception:
        logger.exception("Caption rewrite failed for video %s", video_id)
        raise HTTPException(status_code=500, detail="Caption rewrite failed")

# uvicorn api:app --reload --port 8000
