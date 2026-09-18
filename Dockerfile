FROM python:3.11-slim

WORKDIR /app

RUN apt-get update && apt-get install -y ffmpeg libpq-dev gcc && rm -rf /var/lib/apt/lists/*

COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt

COPY api.py auth.py caption_polish.py database.py glossary.py main.py models.py pipeline.py pricing.py ./

# Runtime data directories (also mounted as volumes in compose)
RUN mkdir -p /app/videos /app/frames /app/texts

ENV WEB_CONCURRENCY=2
CMD ["sh", "-c", "uvicorn api:app --host 0.0.0.0 --port 8000 --workers ${WEB_CONCURRENCY}"]
