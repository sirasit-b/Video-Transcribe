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
