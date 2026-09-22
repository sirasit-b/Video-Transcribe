import argparse
import json
import logging
import os
import re
import shutil
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

from openai import BadRequestError, OpenAI
from dotenv import load_dotenv

import caption_polish
import pricing
from glossary import Glossary

logger = logging.getLogger("pipeline")


# Transcription is OpenAI-only. TRANSCRIBE_MODEL overrides the default model id.
DEFAULT_TRANSCRIBE_MODEL = os.getenv("TRANSCRIBE_MODEL") or "gpt-transcribe"

# Models offered in the UI picker. Only whisper-1 returns timestamps of its own; the
# gpt-*transcribe models are text-only, so we cut cues on silence and take the timing
# from the cut points (see _transcribe_with_openai).
_MODEL_CATALOG: dict[str, dict] = {
	"gpt-transcribe": {
		"provider": "openai",
		"description": "Most accurate on Thai. Cues are cut on silence.",
	},
	"gpt-4o-transcribe": {
		"provider": "openai",
		"description": "Previous-generation OpenAI ASR. Cues are cut on silence.",
	},
	"gpt-4o-mini-transcribe": {
		"provider": "openai",
		"description": "Cheapest OpenAI ASR. Cues are cut on silence.",
	},
	"whisper-1": {
		"provider": "openai",
		"description": "Weakest text, but returns its own segment timings.",
	},
}

# Models whose API hands back segment timings; everything else needs silence cutting.
_OPENAI_NATIVE_TIMESTAMPS = {"whisper-1"}


def _model_label(model_id: str) -> str:
	return model_id.replace("-", " ").title()


def _extra_model_ids(env_var: str, provider: str) -> list[tuple[str, str]]:
	return [
		(model_id.strip(), provider)
		for model_id in os.getenv(env_var, "").split(",")
		if model_id.strip()
	]


def available_transcribe_models() -> list[dict]:
	"""Model ids the transcriber accepts, default first."""
	providers = {model_id: entry["provider"] for model_id, entry in _MODEL_CATALOG.items()}
	for model_id, provider in _extra_model_ids("OPENAI_TRANSCRIBE_MODELS", "openai"):
		providers.setdefault(model_id, provider)
	providers.setdefault(DEFAULT_TRANSCRIBE_MODEL, "openai")

	ids = sorted(providers, key=lambda model_id: model_id != DEFAULT_TRANSCRIBE_MODEL)
	return [
		{
			"id": model_id,
			"label": _model_label(model_id),
			"description": _MODEL_CATALOG.get(model_id, {}).get("description", ""),
			"provider": providers[model_id],
			"is_default": model_id == DEFAULT_TRANSCRIBE_MODEL,
			# Cost/speed for one minute of audio, so the picker can show the trade-off
			# before a model is chosen.
			"estimated_cost_per_minute_usd": pricing.estimate_cost(model_id, 60.0),
			"estimated_seconds_per_minute": pricing.estimate_seconds(model_id, 60.0),
		}
		for model_id in ids
	]


def resolve_transcribe_model(model: str | None) -> str:
	"""Validate a caller-supplied model id, falling back to the configured default."""
	if not model or not model.strip():
		return DEFAULT_TRANSCRIBE_MODEL

	model = model.strip()
	allowed = {entry["id"] for entry in available_transcribe_models()}
	if model not in allowed:
		raise ValueError(f"Unsupported transcription model: {model}")
	return model


def transcribe_model_provider(model: str) -> str:
	"""Which API a model id belongs to. Only 'openai' today."""
	for entry in available_transcribe_models():
		if entry["id"] == model:
			return entry["provider"]
	return "openai"


def _ffmpeg_exe() -> str:
	# Use the system ffmpeg (installed via apt in the Dockerfile) instead of
	# imageio_ffmpeg's bundled binary, which only ships an x86_64 build for Linux —
	# forcing QEMU emulation (and a large slowdown) on arm64 hosts.
	return shutil.which("ffmpeg") or "ffmpeg"


def extract_audio_to_mp3(input_video: str, output_mp3: str | None = None) -> str:
	input_path = Path(input_video)
	if not input_path.exists():
		raise FileNotFoundError(f"Input file not found: {input_path}")

	out_path = Path(output_mp3) if output_mp3 else input_path.with_suffix(".mp3")

	# The ASR models downsample to 16kHz mono anyway, so downmix here to shrink the
	# upload payload and stay clear of the 25MB request cap.
	ffmpeg_exe = _ffmpeg_exe()
	cmd = [
		ffmpeg_exe,
		"-i", str(input_path),
		"-vn",
		"-map", "a",
		"-ac", "1",
		"-ar", "16000",
		"-b:a", "32k",
		"-y",
		str(out_path)
	]
	
	subprocess.run(cmd, check=True, capture_output=False)

	return str(out_path)


def get_video_duration_seconds(video_path: str) -> float:
	input_path = Path(video_path)
	if not input_path.exists():
		raise FileNotFoundError(f"Input file not found: {input_path}")

	ffmpeg_exe = _ffmpeg_exe()
	result = subprocess.run(
		[
			ffmpeg_exe,
			"-i",
			str(input_path),
		],
		check=False,
		capture_output=True,
		text=True,
	)
	match = re.search(r"Duration:\s*(\d+):(\d+):(\d+(?:\.\d+)?)", result.stderr or "")
	if not match:
		raise RuntimeError(f"Could not determine duration for {input_path}")

	hours, minutes, seconds = match.groups()
	duration = (int(hours) * 3600) + (int(minutes) * 60) + float(seconds)
	if duration <= 0:
		raise RuntimeError(f"Video duration must be greater than zero: {input_path}")

	return duration


def extract_evenly_spaced_frames(video_path: str, frame_count: int, output_dir: str) -> list[Path]:
	if frame_count < 1:
		raise ValueError("frame_count must be at least 1")

	video_file = Path(video_path).resolve()
	if not video_file.exists():
		raise FileNotFoundError(f"Input file not found: {video_file}")

	output_base = Path(output_dir).resolve()
	output_base.mkdir(parents=True, exist_ok=True)

	service_url = os.getenv("FRAME_EXTRACTOR_URL")
	if service_url:
		return extract_evenly_spaced_frames_via_service(
			service_url=service_url,
			video_path=video_file,
			frame_count=frame_count,
			output_dir=output_base,
		)

	duration = get_video_duration_seconds(str(video_file))
	ffmpeg_exe = _ffmpeg_exe()

	def _extract_one(index: int) -> tuple[int, Path]:
		timestamp = duration * (index + 0.5) / frame_count
		frame_path = output_base / f"frame_{index + 1:03d}.png"
		cmd = [
			ffmpeg_exe,
			"-noaccurate_seek",
			"-ss",
			f"{timestamp:.3f}",
			"-i",
			str(video_file),
			"-an",
			"-frames:v",
			"1",
			"-q:v",
			"2",
			"-threads",
			"1",
			"-y",
			str(frame_path),
		]
		subprocess.run(cmd, check=True, capture_output=True, text=True)
		return index, frame_path

	# Cap workers at the CPU count so parallel ffmpeg jobs don't thrash the scheduler.
	max_workers = min(frame_count, (os.cpu_count() or 4))
	results: dict[int, Path] = {}
	with ThreadPoolExecutor(max_workers=max_workers) as executor:
		futures = {executor.submit(_extract_one, i): i for i in range(frame_count)}
		for future in as_completed(futures):
			index, path = future.result()
			results[index] = path

	return [results[i] for i in range(frame_count)]


def extract_evenly_spaced_frames_via_service(
	service_url: str,
	video_path: Path,
	frame_count: int,
	output_dir: Path,
) -> list[Path]:
	endpoint = service_url.rstrip("/") + "/extract-frames"
	payload = json.dumps(
		{
			"video_path": str(video_path),
			"frame_count": frame_count,
			"output_dir": str(output_dir),
		}
	).encode("utf-8")
	request = urllib.request.Request(
		endpoint,
		data=payload,
		headers={"Content-Type": "application/json"},
		method="POST",
	)

	try:
		with urllib.request.urlopen(request, timeout=120) as response:
			body = response.read().decode("utf-8")
	except urllib.error.HTTPError as exc:
		error_body = exc.read().decode("utf-8", errors="replace")
		raise RuntimeError(f"Frame extractor service error ({exc.code}): {error_body}") from exc
	except urllib.error.URLError as exc:
		raise RuntimeError(f"Could not reach frame extractor service: {exc.reason}") from exc

	data = json.loads(body)
	frame_paths = [Path(frame_path) for frame_path in data.get("frame_paths", [])]
	if len(frame_paths) != frame_count:
		raise RuntimeError(
			f"Frame extractor service returned {len(frame_paths)} frames, expected {frame_count}"
		)

	return frame_paths


# Auto trim (cutting the silent parts out of a video) runs in the auto-trim-rs
# service, which ports auto-editor's edit decision — per-frame audio peaks, a
# threshold, a margin around every active run, then the short-run smoothing pass.
# There is deliberately no local fallback: a second implementation of that decision
# here would drift from what the service keeps, and a video trimmed two different
# ways depending on deployment is worse than a clear error.
#
# Everything is a job. A three-hour video takes minutes to analyze and render, so
# the service starts the work and answers with a job id; the calls below are all
# quick, and progress comes from polling. Job state lives in the service, not here,
# because the backend runs several uvicorn workers and any of them may take the poll.
AUTO_TRIM_TIMEOUT = int(os.getenv("AUTO_TRIM_TIMEOUT", "120"))


class AutoTrimError(RuntimeError):
	"""A failed auto trim call, carrying the status the service answered with.

	The service separates "you asked for something impossible" (400 for a bad
	threshold or a video past the length limit, 429 when too many jobs are already
	running) from "ffmpeg broke" (5xx), so `status_code` lets the API hand a real
	reason back to the user instead of flattening everything into a 500.
	"""

	def __init__(self, message: str, status_code: int = 502):
		super().__init__(message)
		self.status_code = status_code


def _auto_trim_call(path: str, payload: dict | None = None, method: str = "POST") -> dict:
	service_url = os.getenv("AUTO_TRIM_URL")
	if not service_url:
		raise AutoTrimError("Auto trim service is not configured (set AUTO_TRIM_URL)", 503)

	data = None if payload is None else json.dumps(payload).encode("utf-8")
	request = urllib.request.Request(
		service_url.rstrip("/") + path,
		data=data,
		headers={"Content-Type": "application/json"},
		method=method,
	)

	try:
		with urllib.request.urlopen(request, timeout=AUTO_TRIM_TIMEOUT) as response:
			body = response.read().decode("utf-8")
	except urllib.error.HTTPError as exc:
		detail = exc.read().decode("utf-8", errors="replace").strip()
		raise AutoTrimError(detail or f"Auto trim service error ({exc.code})", exc.code) from exc
	except urllib.error.URLError as exc:
		raise AutoTrimError(f"Could not reach auto trim service: {exc.reason}", 503) from exc

	return json.loads(body)


def _auto_trim_options(options: dict) -> dict:
	"""Drop unset options, so the service applies auto-editor's own defaults."""
	return {key: value for key, value in options.items() if value is not None}


def auto_trim_capabilities() -> dict:
	"""What the trim service found on its machine: the CPU, and whether a GPU encoder
	is usable from inside its container.

	The service tests each candidate by encoding a real clip through the same filter
	chain a render uses, so this reports what *works*, not what ffmpeg was built with.
	"""
	return _auto_trim_call("/capabilities", method="GET")


def auto_trim_estimate(video_path: str, assumed_kept_ratio: float | None = None) -> dict:
	"""How long a trim would take, from the file's shape alone (no decoding).

	Before anything is analyzed the kept length is unknown, so the estimate assumes
	the whole video survives and is therefore an upper bound.
	"""
	payload = {"video_path": str(Path(video_path).resolve())}
	if assumed_kept_ratio is not None:
		payload["assumed_kept_ratio"] = assumed_kept_ratio
	return _auto_trim_call("/estimate", payload)


def auto_trim_start(
	video_path: str,
	mode: str,
	output_path: str | None = None,
	**options,
) -> dict:
	"""Start an `analyze` or `trim` job and return its first status."""
	payload = {
		"mode": mode,
		"video_path": str(Path(video_path).resolve()),
		**_auto_trim_options(options),
	}
	if output_path is not None:
		payload["output_path"] = str(Path(output_path).resolve())
	return _auto_trim_call("/jobs", payload)


def auto_trim_job(job_id: str) -> dict:
	"""Poll one job: phase, progress, ETA, and its result once it is done."""
	return _auto_trim_call(f"/jobs/{job_id}", method="GET")


def auto_trim_cancel(job_id: str) -> dict:
	"""Stop a job and kill the ffmpeg processes it has running."""
	return _auto_trim_call(f"/jobs/{job_id}", method="DELETE")


# The whole pipeline is Thai-only today; the API takes an ISO code.
TRANSCRIBE_LANGUAGE = os.getenv("TRANSCRIBE_LANGUAGE", "th")


def _split_audio_into_chunks(audio_path: str, chunk_seconds: int, out_dir: str) -> list[Path]:
	ffmpeg_exe = _ffmpeg_exe()
	pattern = str(Path(out_dir) / "chunk_%04d.mp3")
	cmd = [
		ffmpeg_exe,
		"-i", audio_path,
		"-f", "segment",
		"-segment_time", str(chunk_seconds),
		"-reset_timestamps", "1",
		"-c", "copy",
		"-y",
		pattern,
	]
	subprocess.run(cmd, check=True, capture_output=True, text=True)
	return sorted(Path(out_dir).glob("chunk_*.mp3"))


def _detect_silences(audio_path: str, noise_db: int = -32, min_silence: float = 0.3) -> list[tuple[float, float]]:
	"""Silence spans in the clip, as (start, end) seconds."""
	result = subprocess.run(
		[
			_ffmpeg_exe(),
			"-i", audio_path,
			"-af", f"silencedetect=noise={noise_db}dB:d={min_silence}",
			"-f", "null",
			"-",
		],
		check=False,
		capture_output=True,
		text=True,
	)
	stderr = result.stderr or ""
	starts = [float(value) for value in re.findall(r"silence_start:\s*([\d.]+)", stderr)]
	ends = [float(value) for value in re.findall(r"silence_end:\s*([\d.]+)", stderr)]
	return list(zip(starts, ends))


def _silence_cue_bounds(
	audio_path: str,
	min_len: float = 2.0,
	max_len: float = 12.0,
) -> list[tuple[float, float]]:
	"""Subtitle-sized (start, end) windows, split in the middle of natural pauses.

	The gpt-*transcribe models return text with no timings at all, so the cut points
	are the only timing information we get — hence cutting on speech pauses rather
	than on a fixed grid.
	"""
	try:
		total = get_video_duration_seconds(audio_path)
	except Exception:
		total = 0.0
	if total <= 0:
		return []

	windows: list[tuple[float, float]] = []
	previous = 0.0
	for silence_start, silence_end in _detect_silences(audio_path):
		cut = (silence_start + min(silence_end, total)) / 2
		if cut - previous >= min_len and cut < total:
			windows.append((previous, cut))
			previous = cut
	# The tail extends the last cue instead of opening its own window. A sliver
	# shorter than min_len gets copied as a truncated final MP3 frame, which the
	# transcription API rejects outright as a corrupted file.
	if windows and total - previous < min_len:
		windows[-1] = (windows[-1][0], total)
	elif total - previous > 0.3:
		windows.append((previous, total))

	# A pauseless stretch still has to be broken up, or one cue would swallow minutes.
	bounded: list[tuple[float, float]] = []
	for start, end in windows:
		# Stop one slice early when a strict max_len grid would leave a sub-min_len
		# remainder; an overlong final cue beats an unusable one.
		while end - start > max_len and end - start - max_len >= min_len:
			bounded.append((start, start + max_len))
			start += max_len
		bounded.append((start, end))
	return bounded


def _slice_audio(audio_path: str, start: float, end: float, out_path: str) -> str:
	subprocess.run(
		[
			_ffmpeg_exe(),
			"-ss", f"{start:.3f}",
			"-t", f"{max(end - start, 0.1):.3f}",
			"-i", audio_path,
			"-c", "copy",
			"-y",
			out_path,
		],
		check=True,
		capture_output=True,
		text=True,
	)
	return out_path


def _openai_native_segments(client: OpenAI, audio_path: str, model: str) -> tuple[str, list[dict]]:
	"""whisper-1 path: let the API return its own segment timings."""
	with open(audio_path, "rb") as handle:
		response = client.audio.transcriptions.create(
			model=model,
			file=handle,
			response_format="verbose_json",
			language=TRANSCRIBE_LANGUAGE,
		)

	data = response.model_dump()
	segments = []
	for item in data.get("segments") or []:
		text = str(item.get("text", "")).strip()
		if text:
			segments.append({"start": float(item["start"]), "end": float(item["end"]), "text": text})
	return str(data.get("text", "")).strip(), segments


def _transcribe_with_openai(
	audio_path: str,
	openai_api_key: str,
	model: str,
	chunk_seconds: int = 300,
	max_workers: int = 8,
) -> dict:
	client = OpenAI(api_key=openai_api_key)
	prompt = os.getenv("OPENAI_TRANSCRIBE_PROMPT") or None

	if model in _OPENAI_NATIVE_TIMESTAMPS:
		try:
			duration = get_video_duration_seconds(audio_path)
		except Exception:
			duration = 0.0

		# whisper-1 caps uploads at 25MB; our 32kbps mono mp3 only hits that around
		# 100 minutes, but chunking also keeps its timestamp drift bounded.
		if duration <= chunk_seconds * 1.5:
			text, segments = _openai_native_segments(client, audio_path, model)
			return {"text": text, "segments": segments, "usage": {"requests": 1}}

		with tempfile.TemporaryDirectory() as chunk_dir:
			chunks = _split_audio_into_chunks(audio_path, chunk_seconds, chunk_dir)
			offsets: list[float] = []
			running = 0.0
			for chunk in chunks:
				offsets.append(running)
				try:
					running += get_video_duration_seconds(str(chunk))
				except Exception:
					running += chunk_seconds

			results: list[tuple[str, list[dict]] | None] = [None] * len(chunks)
			with ThreadPoolExecutor(max_workers=max_workers) as executor:
				futures = {
					executor.submit(_openai_native_segments, client, str(chunk), model): idx
					for idx, chunk in enumerate(chunks)
				}
				for future in as_completed(futures):
					results[futures[future]] = future.result()

		texts: list[str] = []
		merged: list[dict] = []
		for idx, result in enumerate(results):
			if result is None:
				continue
			text, segments = result
			if text:
				texts.append(text)
			for seg in segments:
				merged.append(
					{
						"start": seg["start"] + offsets[idx],
						"end": seg["end"] + offsets[idx],
						"text": seg["text"],
					}
				)
		return {"text": " ".join(texts), "segments": merged, "usage": {"requests": len(chunks)}}

	# Text-only models: one request per silence-bounded cue, timing from the cut points.
	windows = _silence_cue_bounds(audio_path)
	if not windows:
		return {"text": "", "segments": [], "usage": {"requests": 0}}

	with tempfile.TemporaryDirectory() as cue_dir:
		def _transcribe_window(index: int) -> tuple[int, str]:
			start, end = windows[index]
			piece = str(Path(cue_dir) / f"cue_{index:05d}.mp3")
			_slice_audio(audio_path, start, end, piece)
			try:
				with open(piece, "rb") as handle:
					response = client.audio.transcriptions.create(
						model=model,
						file=handle,
						response_format="json",
						language=TRANSCRIBE_LANGUAGE,
						prompt=prompt,
					)
			except BadRequestError:
				# A window the API refuses costs us that one cue, not the whole file.
				logger.warning(
					"Dropping cue %d (%.3f-%.3f) of %s: rejected by %s",
					index, start, end, audio_path, model,
				)
				return index, ""
			return index, str(response.text or "").strip()

		texts_by_index: dict[int, str] = {}
		with ThreadPoolExecutor(max_workers=max_workers) as executor:
			futures = [executor.submit(_transcribe_window, index) for index in range(len(windows))]
			for future in as_completed(futures):
				index, text = future.result()
				texts_by_index[index] = text

	segments = []
	for index, (start, end) in enumerate(windows):
		text = texts_by_index.get(index, "")
		if text:
			segments.append({"start": start, "end": end, "text": text})

	return {
		"text": " ".join(seg["text"] for seg in segments),
		"segments": segments,
		"usage": {"requests": len(windows)},
	}


def transcribe_audio_file(
	audio_path: str,
	openai_api_key: str,
	# whisper-1 caps uploads at 25MB, which our 32kbps mono mp3 only reaches around
	# 100 minutes; chunking also bounds its timestamp drift and keeps the requests
	# parallel. The text-only models ignore this and cut on silence instead.
	chunk_seconds: int = 300,
	model: str | None = None,
) -> dict:
	model = resolve_transcribe_model(model)
	if not openai_api_key:
		raise ValueError(f"OPENAI_API_KEY is required for {model}")

	try:
		audio_seconds = get_video_duration_seconds(audio_path)
	except Exception:
		audio_seconds = 0.0

	result = _transcribe_with_openai(
		audio_path,
		openai_api_key,
		model,
		chunk_seconds=chunk_seconds,
	)
	result["audio_seconds"] = audio_seconds
	return result


REWRITE_MODEL = os.getenv("REWRITE_MODEL", "gpt-5")


def rewrite_caption_segments(
	segments: list[dict], openai_api_key: str
) -> tuple[list[dict], dict]:
	"""Fix typos/spelling in caption text using an LLM while keeping timing unchanged.

	This is the paid, non-deterministic proofreader. For the free, repeatable pass
	see `polish_captions`. Returns the rewritten segments plus a cost report, because
	unlike the deterministic pass this one shows up on the bill.
	"""
	if not segments:
		return [], pricing.report_llm_run(REWRITE_MODEL, 0.0)

	started = time.monotonic()
	client = OpenAI(api_key=openai_api_key)
	numbered = [{"i": idx, "text": seg["text"]} for idx, seg in enumerate(segments)]

	system_prompt = (
		"You are a Thai subtitle proofreader. Fix spelling mistakes, mis-transcribed words, "
		"and obvious ASR errors in each caption line. Preserve meaning and keep each line roughly "
		"the same length. Do not merge, split, add, or remove lines. Return only valid JSON."
	)
	user_prompt = (
		"Correct the 'text' of each item below. Return a JSON object of the form "
		'{"items": [{"i": <index>, "text": "<corrected text>"}, ...]} '
		"with exactly one entry per input index and the same indexes.\n\n"
		+ json.dumps(numbered, ensure_ascii=False)
	)

	response = client.chat.completions.create(
		model=REWRITE_MODEL,
		messages=[
			{"role": "system", "content": system_prompt},
			{"role": "user", "content": user_prompt},
		],
		response_format={"type": "json_object"},
	)

	data = json.loads(response.choices[0].message.content)
	corrections = {int(item["i"]): str(item["text"]).strip() for item in data.get("items", [])}

	rewritten = []
	for idx, seg in enumerate(segments):
		rewritten.append(
			{
				"start": seg["start"],
				"end": seg["end"],
				"text": corrections.get(idx, seg["text"]).strip() or seg["text"],
			}
		)

	usage = getattr(response, "usage", None)
	stats = pricing.report_llm_run(
		REWRITE_MODEL,
		elapsed_seconds=time.monotonic() - started,
		input_tokens=int(getattr(usage, "prompt_tokens", 0) or 0),
		output_tokens=int(getattr(usage, "completion_tokens", 0) or 0),
	)
	return rewritten, stats


def _format_srt_timestamp(seconds: float) -> str:
	total_ms = round(seconds * 1000)
	hours, rem = divmod(total_ms, 3_600_000)
	minutes, rem = divmod(rem, 60_000)
	secs, ms = divmod(rem, 1000)
	return f"{hours:02d}:{minutes:02d}:{secs:02d},{ms:03d}"


def segments_to_srt(segments: list[dict]) -> str:
	lines = []
	for index, seg in enumerate(segments, start=1):
		lines.append(str(index))
		lines.append(f"{_format_srt_timestamp(seg['start'])} --> {_format_srt_timestamp(seg['end'])}")
		lines.append(seg["text"].strip())
		lines.append("")
	return "\n".join(lines)


_SRT_TIME_RE = re.compile(
	r"(\d{1,2}):(\d{2}):(\d{2})[,.](\d{1,3})\s*-->\s*(\d{1,2}):(\d{2}):(\d{2})[,.](\d{1,3})"
)


def parse_srt(content: str) -> list[dict]:
	"""Parse SRT text into [{start, end, text}, ...]. Ignores index numbers and
	tolerates missing/blank lines; raises ValueError if no cues are found."""

	def to_seconds(h: str, m: str, s: str, ms: str) -> float:
		return int(h) * 3600 + int(m) * 60 + int(s) + int(ms.ljust(3, "0")) / 1000

	normalized = content.replace("﻿", "").replace("\r\n", "\n").replace("\r", "\n")
	blocks = re.split(r"\n\s*\n", normalized.strip())

	segments = []
	for block in blocks:
		lines = [line for line in block.split("\n") if line.strip()]
		if not lines:
			continue

		time_line_index = next((i for i, line in enumerate(lines) if _SRT_TIME_RE.search(line)), None)
		if time_line_index is None:
			continue

		match = _SRT_TIME_RE.search(lines[time_line_index])
		start = to_seconds(*match.group(1, 2, 3, 4))
		end = to_seconds(*match.group(5, 6, 7, 8))
		text = " ".join(lines[time_line_index + 1:]).strip()
		if not text:
			continue

		segments.append({"start": start, "end": end, "text": text})

	if not segments:
		raise ValueError("No valid SRT cues found in the uploaded file")

	return segments


def segments_to_vtt(segments: list[dict]) -> str:
	def fmt(seconds: float) -> str:
		return _format_srt_timestamp(seconds).replace(",", ".")

	lines = ["WEBVTT", ""]
	for seg in segments:
		lines.append(f"{fmt(seg['start'])} --> {fmt(seg['end'])}")
		lines.append(seg["text"].strip())
		lines.append("")
	return "\n".join(lines)


def load_video_paths(video_paths: list[str] | None, video_list_file: str | None) -> list[str]:
	paths: list[str] = []

	if video_list_file:
		with open(video_list_file, "r", encoding="utf-8") as handle:
			for line in handle:
				item = line.strip()
				if item and not item.startswith("#"):
					paths.append(item)

	if video_paths:
		paths.extend(video_paths)

	if not paths:
		raise ValueError("Provide at least one video path or a video list file")

	return paths


def polish_captions(
	segments: list[dict],
	glossary: Glossary | None = None,
	split_lines: bool = True,
) -> tuple[list[dict], dict]:
	"""Deterministic clean-up pass: word system, spacing, line splitting, timing.

	Separate from `rewrite_caption_segments`, which is the (paid, non-deterministic)
	LLM proofreader. This one costs nothing and always produces the same output for
	the same input, so it can run automatically after every transcription.
	"""
	return caption_polish.polish_segments(segments, glossary=glossary, split_lines=split_lines)


def estimate_video_run(video_path: str, model: str | None = None) -> dict:
	"""Projected duration and cost for transcribing a video, before running it."""
	model = resolve_transcribe_model(model)
	try:
		audio_seconds = get_video_duration_seconds(video_path)
	except Exception:
		audio_seconds = 0.0
	return pricing.estimate_run(model, transcribe_model_provider(model), audio_seconds)


def process_video(
	video_path: str,
	openai_api_key: str,
	output_dir: str | None = None,
	model: str | None = None,
	polish: bool = True,
	glossary: Glossary | None = None,
) -> dict:
	video_file = Path(video_path)
	if not video_file.exists():
		raise FileNotFoundError(f"Input file not found: {video_file}")

	output_base = Path(output_dir) if output_dir else video_file.parent
	output_base.mkdir(parents=True, exist_ok=True)

	model = resolve_transcribe_model(model)
	started = time.monotonic()

	with tempfile.TemporaryDirectory() as temp_dir:
		audio_path = Path(temp_dir) / f"{video_file.stem}.mp3"
		extract_audio_to_mp3(str(video_file), str(audio_path))
		result = transcribe_audio_file(
			str(audio_path),
			openai_api_key,
			model=model,
		)

	segments = result["segments"]
	text = result["text"]
	polish_report: dict | None = None
	if polish:
		segments, polish_report = polish_captions(segments, glossary=glossary)
		# Keep the plain transcript consistent with the captions the user will see.
		text = " ".join(segment["text"] for segment in segments)

	# The report covers the whole call — audio extraction included — because that is
	# the wait the user actually experiences.
	stats = pricing.report_run(
		model=model,
		provider=transcribe_model_provider(model),
		audio_seconds=result.get("audio_seconds", 0.0),
		elapsed_seconds=time.monotonic() - started,
		usage=result.get("usage"),
	)

	text_path = output_base / f"{video_file.stem}.txt"
	srt_path = output_base / f"{video_file.stem}.srt"
	text_path.write_text(text, encoding="utf-8")
	srt_path.write_text(segments_to_srt(segments), encoding="utf-8")

	return {
		"text": text,
		"segments": segments,
		"model": model,
		"stats": stats,
		"polish_report": polish_report,
		"text_path": text_path,
		"srt_path": srt_path,
	}


def main() -> None:
	parser = argparse.ArgumentParser(description="Extract audio from video files and transcribe them with OpenAI")
	parser.add_argument("video_paths", nargs="*", help="Paths to input video files")
	parser.add_argument("-l", "--video-list", dest="video_list_file", help="Text file containing one video path per line")
	parser.add_argument("-o", "--output-dir", dest="output_dir", help="Directory where transcripts are saved")
	parser.add_argument(
		"-m",
		"--model",
		dest="model",
		help="Model to transcribe with (default: %s)" % DEFAULT_TRANSCRIBE_MODEL,
	)
	parser.add_argument(
		"--no-polish",
		dest="polish",
		action="store_false",
		help="Skip the glossary/line-splitting clean-up pass",
	)
	parser.add_argument(
		"--estimate-only",
		dest="estimate_only",
		action="store_true",
		help="Print the projected time and cost without transcribing",
	)
	args = parser.parse_args()

	load_dotenv()
	model = resolve_transcribe_model(args.model)
	openai_key = os.getenv("OPENAI_API_KEY")
	if not openai_key and not args.estimate_only:
		raise ValueError("Set OPENAI_API_KEY in your .env file")

	video_paths = load_video_paths(args.video_paths, args.video_list_file)

	if args.estimate_only:
		for video_path in video_paths:
			estimate = estimate_video_run(video_path, model)
			print(
				f"{video_path}: ~{estimate['estimated_seconds']:.0f}s, "
				f"~${estimate['estimated_cost_usd']:.4f} "
				f"({estimate['audio_seconds']:.0f}s audio, {estimate['basis']})"
			)
		return

	grand_total = 0.0
	for video_path in video_paths:
		result = process_video(
			video_path,
			openai_key,
			args.output_dir,
			model=model,
			polish=args.polish,
		)
		print(f"Saved transcript: {result['text_path']}")
		print(f"Saved captions: {result['srt_path']}")

		stats = result["stats"]
		grand_total += stats["estimated_cost_usd"]
		print(
			f"  {stats['model']}: {stats['elapsed_seconds']:.1f}s for "
			f"{stats['audio_seconds']:.0f}s of audio "
			f"({stats.get('realtime_factor', 0)}x realtime), "
			f"~${stats['estimated_cost_usd']:.4f} [{stats['cost_basis']}]"
		)
		report = result.get("polish_report")
		if report:
			print(
				f"  polish: {report['correction_count']} word fixes, "
				f"{report['segments_in']} -> {report['segments_out']} lines, "
				f"{len(report['timing_issues'])} timing fixes "
				f"(tokenizer: {report['tokenizer']})"
			)
			for rule in report["by_rule"][:10]:
				variants = ", ".join(f"{before} x{count}" for before, count in rule["variants"])
				print(f"    {rule['after']} <- {variants}")

	if len(video_paths) > 1:
		print(f"Total estimated cost: ~${grand_total:.4f}")


if __name__ == "__main__":
	main()
