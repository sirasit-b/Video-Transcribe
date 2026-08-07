import os
import secrets
from datetime import datetime, timedelta, timezone

import bcrypt
import jwt
from fastapi import Depends, HTTPException, Query, status
from fastapi.security import OAuth2PasswordBearer
from sqlalchemy.orm import Session

import models
from database import get_db

APP_ENV = os.getenv("APP_ENV", "development")
INSECURE_DEFAULT_SECRET = "change-me-in-production"
SECRET_KEY = os.getenv("JWT_SECRET_KEY", INSECURE_DEFAULT_SECRET)
ALGORITHM = "HS256"
ACCESS_TOKEN_EXPIRE_MINUTES = int(os.getenv("ACCESS_TOKEN_EXPIRE_MINUTES", "1440"))
RESET_TOKEN_EXPIRE_MINUTES = int(os.getenv("RESET_TOKEN_EXPIRE_MINUTES", "30"))

if APP_ENV == "production" and SECRET_KEY == INSECURE_DEFAULT_SECRET:
    raise RuntimeError(
        "JWT_SECRET_KEY is still the insecure default. Set a real random secret "
        "(see .env.example) before running with APP_ENV=production."
    )

oauth2_scheme = OAuth2PasswordBearer(tokenUrl="api/auth/login")
oauth2_scheme_optional = OAuth2PasswordBearer(tokenUrl="api/auth/login", auto_error=False)


def hash_password(password: str) -> str:
    return bcrypt.hashpw(password.encode("utf-8"), bcrypt.gensalt()).decode("utf-8")


def verify_password(password: str, hashed: str) -> bool:
    return bcrypt.checkpw(password.encode("utf-8"), hashed.encode("utf-8"))


def create_access_token(subject: str) -> str:
    expire = datetime.now(timezone.utc) + timedelta(minutes=ACCESS_TOKEN_EXPIRE_MINUTES)
    payload = {"sub": subject, "exp": expire}
    return jwt.encode(payload, SECRET_KEY, algorithm=ALGORITHM)


def generate_reset_token() -> tuple[str, str, datetime]:
    """Returns (plaintext_token, hashed_token, expires_at). The plaintext token is
    shown to the admin once and handed to the user out-of-band; only the hash is stored."""
    token = secrets.token_urlsafe(24)
    expires_at = datetime.now(timezone.utc) + timedelta(minutes=RESET_TOKEN_EXPIRE_MINUTES)
    return token, hash_password(token), expires_at


def _resolve_user_from_token(token: str | None, db: Session) -> models.User:
    credentials_exception = HTTPException(
        status_code=status.HTTP_401_UNAUTHORIZED,
        detail="Could not validate credentials",
        headers={"WWW-Authenticate": "Bearer"},
    )
    if not token:
        raise credentials_exception
    try:
        payload = jwt.decode(token, SECRET_KEY, algorithms=[ALGORITHM])
        username = payload.get("sub")
        if not username:
            raise credentials_exception
    except jwt.PyJWTError:
        raise credentials_exception

    user = db.query(models.User).filter(models.User.username == username).first()
    if not user:
        raise credentials_exception
    return user


def get_current_user(
    token: str = Depends(oauth2_scheme),
    db: Session = Depends(get_db),
) -> models.User:
    return _resolve_user_from_token(token, db)


def get_current_user_for_media(
    header_token: str | None = Depends(oauth2_scheme_optional),
    query_token: str | None = Query(default=None, alias="token"),
    db: Session = Depends(get_db),
) -> models.User:
    """Same as get_current_user, but also accepts the JWT as a `token` query param
    since <video>/<track>/<img> tags can't attach an Authorization header."""
    return _resolve_user_from_token(header_token or query_token, db)


def require_admin(current_user: models.User = Depends(get_current_user)) -> models.User:
    if current_user.role != "admin":
        raise HTTPException(status_code=status.HTTP_403_FORBIDDEN, detail="Admin privileges required")
    return current_user
