from sqlalchemy import (
    Column,
    ForeignKey,
    Integer,
    String,
    Text,
    DateTime,
    Boolean,
    JSON,
    UniqueConstraint,
)
from sqlalchemy.sql import func
from database import Base

class User(Base):
    __tablename__ = "users"

    id = Column(Integer, primary_key=True, index=True)
    username = Column(String, nullable=False, unique=True, index=True)
    hashed_password = Column(String, nullable=False)
    role = Column(String, nullable=False, default="user")
    reset_token_hash = Column(String, nullable=True)
    reset_token_expires_at = Column(DateTime(timezone=True), nullable=True)
    created_at = Column(DateTime(timezone=True), server_default=func.now())

class Project(Base):
    __tablename__ = "projects"

    id = Column(Integer, primary_key=True, index=True)
    owner_id = Column(Integer, ForeignKey("users.id"), nullable=False, index=True)
    name = Column(String, nullable=False)
    created_at = Column(DateTime(timezone=True), server_default=func.now())

class SyncSession(Base):
    """Several recordings of one moment, cut as one.

    A camera on the person and a capture of their screen — or a second screen, or
    a second camera — are one take recorded several times. They are brought to the
    same loudness, lined up by what their microphones share, and cut on one
    timeline, because an edit decided separately for each would put the cuts in
    different places and the recordings would never fit together again.
    """

    __tablename__ = "sync_sessions"

    id = Column(Integer, primary_key=True, index=True)
    owner_id = Column(Integer, ForeignKey("users.id"), nullable=False, index=True)
    name = Column(String, nullable=False)
    # Whose clock the session runs on; every other member is placed against it.
    # The first recording added, unless it is changed.
    reference_video_id = Column(Integer, nullable=True)
    # The lining-up or the cut currently running, if any. Kept here rather than in
    # the request that started it, so a reload picks the polling back up.
    job_id = Column(String, nullable=True)
    # What the last run found: where each recording sits, how loud it was, how far
    # it had to move, and what the cut came to.
    result = Column(JSON, nullable=True)
    created_at = Column(DateTime(timezone=True), server_default=func.now())


class Video(Base):
    __tablename__ = "videos"

    id = Column(Integer, primary_key=True, index=True)
    owner_id = Column(Integer, ForeignKey("users.id"), nullable=True, index=True)
    project_id = Column(Integer, ForeignKey("projects.id"), nullable=True, index=True)
    position = Column(Integer, nullable=False, default=0)
    filename = Column(String, nullable=False, unique=True)
    original_name = Column(String, nullable=False)
    has_transcript = Column(Boolean, default=False)
    transcript_text = Column(Text, nullable=True)
    caption_segments = Column(JSON, nullable=True)
    # Elapsed time, token usage and estimated cost of the last transcription run.
    transcribe_stats = Column(JSON, nullable=True)
    # Highlighted word fixes and timing repairs from the last clean-up pass.
    polish_report = Column(JSON, nullable=True)
    # Time and token cost of the last LLM proofreading run.
    rewrite_stats = Column(JSON, nullable=True)
    # Path of the last auto-trimmed render, relative to the videos directory.
    trim_filename = Column(String, nullable=True)
    # The auto trim job currently running for this video, if any. Kept here rather
    # than in the request that started it, so a reload (or another uvicorn worker)
    # can pick the polling back up and file the result.
    trim_job_id = Column(String, nullable=True)
    # What that trim cut: durations, frame counts, settings, timings, and the
    # loudness envelope before and after (downsampled, so the panel can draw it
    # without re-analyzing). The segment list is kept separately.
    trim_result = Column(JSON, nullable=True)
    # Every kept range of the last trim, on the timeline's own timebase. Large
    # enough to be worth keeping out of VideoResponse, and only read when an edit
    # is exported to an editor.
    trim_segments = Column(JSON, nullable=True)
    # The session this recording belongs to, when it is one of several views of
    # the same moment.
    sync_session_id = Column(Integer, nullable=True, index=True)
    # The other recording of the same session — a camera take and a screen capture
    # are two views of one moment, and once they have been lined up they are cut
    # together. Set on both rows, each pointing at the other.
    pair_video_id = Column(Integer, nullable=True)
    # What lining them up found: the gap between them, how certain that is, and
    # whether the two recorders kept the same time. Kept so a later trim can reuse
    # it, and so the panel can show it without measuring again.
    sync_result = Column(JSON, nullable=True)
    is_deleted = Column(Boolean, nullable=False, default=False)
    deleted_at = Column(DateTime(timezone=True), nullable=True)
    created_at = Column(DateTime(timezone=True), server_default=func.now())

class GlossaryRule(Base):
    """A user-added or user-overridden entry in the ASR correction word system.

    Built-in rules live in `glossary.py`; a row here either adds a new rule or, by
    reusing a built-in's `rule_id`, replaces it. `is_disabled` switches a rule off
    without deleting it, which is how a built-in gets turned off.
    """

    __tablename__ = "glossary_rules"
    # One row per rule per owner, so an upsert cannot race itself into duplicates.
    __table_args__ = (UniqueConstraint("owner_id", "rule_id", name="uq_glossary_owner_rule"),)

    id = Column(Integer, primary_key=True, index=True)
    owner_id = Column(Integer, ForeignKey("users.id"), nullable=True, index=True)
    # Matches the id format built by glossary.Rule: "<category>:<slug>".
    rule_id = Column(String, nullable=False, index=True)
    right = Column(String, nullable=False)
    wrong = Column(JSON, nullable=False, default=list)
    category = Column(String, nullable=False, default="term")
    note = Column(Text, nullable=True)
    is_protected = Column(Boolean, nullable=False, default=False)
    is_disabled = Column(Boolean, nullable=False, default=False)
    created_at = Column(DateTime(timezone=True), server_default=func.now())
    updated_at = Column(DateTime(timezone=True), onupdate=func.now())

class CaptionReplacement(Base):
    """One find & replace run over a video's captions, kept as an audit trail.

    Rows are never rewritten: re-running the same pair appends a second row, so the
    history reads as what was actually done and when.
    """

    __tablename__ = "caption_replacements"

    id = Column(Integer, primary_key=True, index=True)
    video_id = Column(Integer, ForeignKey("videos.id"), nullable=False, index=True)
    owner_id = Column(Integer, ForeignKey("users.id"), nullable=True, index=True)
    # The searched text and its replacement: the "before" and "after" of the edit.
    find_text = Column(Text, nullable=False)
    replace_text = Column(Text, nullable=False)
    match_case = Column(Boolean, nullable=False, default=False)
    # How much the run touched, so a history line stands on its own.
    occurrences = Column(Integer, nullable=False, default=0)
    segments_changed = Column(Integer, nullable=False, default=0)
    # Which cues were rewritten, for a targeted (single-line) replace.
    segment_indexes = Column(JSON, nullable=False, default=list)
    # Set when the pair was also taught to the word system; holds the rule id.
    glossary_rule_id = Column(String, nullable=True)
    created_at = Column(DateTime(timezone=True), server_default=func.now())
