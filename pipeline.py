import argparse
import json
import logging
import os
import re
import shutil
import subprocess
import tempfile
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

from google import genai
from google.genai import errors as genai_errors
from google.genai import types
from openai import BadRequestError, OpenAI
from dotenv import load_dotenv

logger = logging.getLogger("pipeline")


# gemini-3.1-pro-preview is the stronger model but is unavailable on the Gemini free
# tier (quota 0, every request 429s), so the default is the flash model that works on
# an unbilled key. Any override must support audio input AND JSON mode — the dedicated
# gemini-*-transcribe family does not.
GEMINI_TRANSCRIBE_MODEL = os.getenv("GEMINI_TRANSCRIBE_MODEL", "gemini-3.5-flash")

# TRANSCRIBE_MODEL picks the default across providers; GEMINI_TRANSCRIBE_MODEL is the
# older, Gemini-only name and still works.
DEFAULT_TRANSCRIBE_MODEL = os.getenv("TRANSCRIBE_MODEL") or GEMINI_TRANSCRIBE_MODEL

# Models offered in the UI picker. Only whisper-1 returns timestamps of its own on the
# OpenAI side; the gpt-*transcribe models are text-only, so we cut cues on silence and
# take the timing from the cut points (see _transcribe_with_openai).
_MODEL_CATALOG: dict[str, dict] = {
	"gemini-3.5-flash": {
		"provider": "gemini",
		"description": "Fast and cheap. Works on a free-tier key.",
	},
	"gemini-3.1-pro-preview": {
		"provider": "gemini",
		"description": "Higher accuracy, slower. Needs a billing-enabled key.",
	},
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
	return model_id.replace("gemini-", "gemini ").replace("-", " ").title()


def _extra_model_ids(env_var: str, provider: str) -> list[tuple[str, str]]:
	return [
		(model_id.strip(), provider)
		for model_id in os.getenv(env_var, "").split(",")
		if model_id.strip()
	]


def available_transcribe_models() -> list[dict]:
	"""Model ids the transcriber accepts, default first."""
	providers = {model_id: entry["provider"] for model_id, entry in _MODEL_CATALOG.items()}
	for model_id, provider in _extra_model_ids("GEMINI_TRANSCRIBE_MODELS", "gemini"):
		providers.setdefault(model_id, provider)
	for model_id, provider in _extra_model_ids("OPENAI_TRANSCRIBE_MODELS", "openai"):
		providers.setdefault(model_id, provider)
	# An unknown default is assumed to be Gemini — that env var predates OpenAI support.
	providers.setdefault(DEFAULT_TRANSCRIBE_MODEL, "gemini")

	ids = sorted(providers, key=lambda model_id: model_id != DEFAULT_TRANSCRIBE_MODEL)
	return [
		{
			"id": model_id,
			"label": _model_label(model_id),
			"description": _MODEL_CATALOG.get(model_id, {}).get("description", ""),
			"provider": providers[model_id],
			"is_default": model_id == DEFAULT_TRANSCRIBE_MODEL,
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
	"""Which API a model id belongs to: 'gemini' or 'openai'."""
	for entry in available_transcribe_models():
		if entry["id"] == model:
			return entry["provider"]
	return "gemini"


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

	# Gemini downsamples audio to 16kHz mono anyway, so downmix here to shrink the upload payload.
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


_TRANSCRIPT_SCHEMA = {
	"type": "object",
	"properties": {
		"segments": {
			"type": "array",
			"items": {
				"type": "object",
				"properties": {
					"start": {"type": "string"},
					"end": {"type": "string"},
					"text": {"type": "string"},
				},
				"required": ["start", "end", "text"],
				"propertyOrdering": ["start", "end", "text"],
			},
		},
	},
	"required": ["segments"],
}

# The whole pipeline is Thai-only today; both APIs take the same ISO code.
TRANSCRIBE_LANGUAGE = os.getenv("TRANSCRIBE_LANGUAGE", "th")

_TRANSCRIBE_PROMPT = """Transcribe the Thai speech in this audio clip into subtitle segments.
Rules:
- Transcribe verbatim in Thai script. Never translate, summarise, or add commentary.
- Split on natural pauses; keep each segment under roughly 15 words.
- 'start' and 'end' are timestamps measured from the beginning of THIS clip, formatted as MM:SS.mmm (for example 00:03.480).
- Segments must be in chronological order and must not overlap.
- Emit nothing for silence, music, or background noise.
- If the clip contains no speech, return an empty segments list."""


def _parse_timestamp(value: object) -> float:
	"""Accept 'H:MM:SS.mmm', 'MM:SS.mmm', 'SS.mmm', or a raw number of seconds."""
	if isinstance(value, (int, float)):
		return float(value)

	raw = str(value or "").strip()
	if not raw:
		return 0.0

	try:
		numbers = [float(part) for part in raw.split(":")]
	except ValueError:
		return 0.0

	total = 0.0
	for number in numbers:
		total = total * 60 + number
	return total


# A model is added here the first time it rejects thinking_config, then skipped for
# the rest of the process so we only pay for that discovery once per model.
_THINKING_UNSUPPORTED: set[str] = set()


def _generation_config(with_thinking: bool) -> types.GenerateContentConfig:
	return types.GenerateContentConfig(
		response_mime_type="application/json",
		response_schema=_TRANSCRIPT_SCHEMA,
		# Transcription needs no deliberation, and LOW keeps per-chunk latency near what
		# whisper-1 cost us. Temperature is deliberately left at the Gemini 3 default —
		# Google warns that lowering it on this family induces looping.
		thinking_config=types.ThinkingConfig(thinking_level="LOW") if with_thinking else None,
	)


def _transcribe_chunk(client: genai.Client, audio_path: str, model: str) -> tuple[str, list[dict]]:
	# Chunks are 32kbps mono mp3, so even a 5-minute one stays far below the 20MB
	# inline-request ceiling — no Files API round trip needed.
	audio_bytes = Path(audio_path).read_bytes()

	contents = [
		types.Part.from_bytes(data=audio_bytes, mime_type="audio/mp3"),
		_TRANSCRIBE_PROMPT,
	]

	with_thinking = model not in _THINKING_UNSUPPORTED
	try:
		response = client.models.generate_content(
			model=model,
			contents=contents,
			config=_generation_config(with_thinking=with_thinking),
		)
	except genai_errors.ClientError as exc:
		# The dedicated *-transcribe models reject thinking_config with a 400. Learn that
		# from the response instead of hard-coding a model allowlist, so the model stays
		# a free choice.
		if not with_thinking or "hinking" not in str(exc):
			raise
		_THINKING_UNSUPPORTED.add(model)
		response = client.models.generate_content(
			model=model,
			contents=contents,
			config=_generation_config(with_thinking=False),
		)

	body = response.text
	if not body:
		# Empty candidate — usually a safety block or a chunk that is pure silence.
		return "", []

	data = json.loads(body)

	segments: list[dict] = []
	previous_end = 0.0
	for item in data.get("segments", []):
		text = str(item.get("text", "")).strip()
		if not text:
			continue
		# Clamping to previous_end repairs the occasional overlapping span.
		start = max(_parse_timestamp(item.get("start")), previous_end)
		end = _parse_timestamp(item.get("end"))
		if end <= start:
			end = start + 1.0
		segments.append({"start": start, "end": end, "text": text})
		previous_end = end

	return " ".join(seg["text"] for seg in segments), segments


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
			return {"text": text, "segments": segments}

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
		return {"text": " ".join(texts), "segments": merged}

	# Text-only models: one request per silence-bounded cue, timing from the cut points.
	windows = _silence_cue_bounds(audio_path)
	if not windows:
		return {"text": "", "segments": []}

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

	return {"text": " ".join(seg["text"] for seg in segments), "segments": segments}


def transcribe_audio_file(
	audio_path: str,
	gemini_api_key: str | None = None,
	# whisper-1 forced 120s chunks to stay under its 25MB upload cap. Gemini accepts
	# hours of audio, so we chunk purely to bound timestamp drift and to keep the
	# requests parallel — 5 minutes cuts the request count 2.5x versus the old default.
	chunk_seconds: int = 300,
	max_workers: int = 4,
	model: str | None = None,
	openai_api_key: str | None = None,
) -> dict:
	model = resolve_transcribe_model(model)

	if transcribe_model_provider(model) == "openai":
		if not openai_api_key:
			raise ValueError(f"OPENAI_API_KEY is required for {model}")
		return _transcribe_with_openai(
			audio_path,
			openai_api_key,
			model,
			chunk_seconds=chunk_seconds,
		)

	if not gemini_api_key:
		raise ValueError(f"GEMINI_API_KEY is required for {model}")
	client = genai.Client(api_key=gemini_api_key)

	try:
		duration = get_video_duration_seconds(audio_path)
	except Exception:
		duration = 0.0

	# Short clips: one request avoids split/merge overhead.
	if duration <= chunk_seconds * 1.5:
		text, segments = _transcribe_chunk(client, audio_path, model)
		return {"text": text, "segments": segments}

	# Long clips: split and transcribe chunks concurrently to parallelize API latency.
	with tempfile.TemporaryDirectory() as chunk_dir:
		chunks = _split_audio_into_chunks(audio_path, chunk_seconds, chunk_dir)
		if len(chunks) <= 1:
			text, segments = _transcribe_chunk(client, audio_path, model)
			return {"text": text, "segments": segments}

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
				executor.submit(_transcribe_chunk, client, str(chunk), model): idx
				for idx, chunk in enumerate(chunks)
			}
			for future in as_completed(futures):
				results[futures[future]] = future.result()

	texts: list[str] = []
	merged_segments: list[dict] = []
	for idx, result in enumerate(results):
		if result is None:
			continue
		text, segments = result
		offset = offsets[idx]
		if text.strip():
			texts.append(text.strip())
		for seg in segments:
			merged_segments.append(
				{
					"start": seg["start"] + offset,
					"end": seg["end"] + offset,
					"text": seg["text"],
				}
			)

	return {"text": " ".join(texts), "segments": merged_segments}


def rewrite_caption_segments(segments: list[dict], openai_api_key: str) -> list[dict]:
	"""Fix typos/spelling in caption text using an LLM while keeping timing unchanged.

	Still runs on OpenAI; only transcription moved to Gemini.
	"""
	if not segments:
		return []

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
		model="gpt-5",
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
	return rewritten


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


def process_video(
	video_path: str,
	gemini_api_key: str | None = None,
	output_dir: str | None = None,
	model: str | None = None,
	openai_api_key: str | None = None,
) -> dict:
	video_file = Path(video_path)
	if not video_file.exists():
		raise FileNotFoundError(f"Input file not found: {video_file}")

	output_base = Path(output_dir) if output_dir else video_file.parent
	output_base.mkdir(parents=True, exist_ok=True)

	with tempfile.TemporaryDirectory() as temp_dir:
		audio_path = Path(temp_dir) / f"{video_file.stem}.mp3"
		extract_audio_to_mp3(str(video_file), str(audio_path))
		result = transcribe_audio_file(
			str(audio_path),
			gemini_api_key,
			model=model,
			openai_api_key=openai_api_key,
		)

	text_path = output_base / f"{video_file.stem}.txt"
	srt_path = output_base / f"{video_file.stem}.srt"
	text_path.write_text(result["text"], encoding="utf-8")
	srt_path.write_text(segments_to_srt(result["segments"]), encoding="utf-8")

	return {
		"text": result["text"],
		"segments": result["segments"],
		"model": resolve_transcribe_model(model),
		"text_path": text_path,
		"srt_path": srt_path,
	}


def main() -> None:
	parser = argparse.ArgumentParser(description="Extract audio from video files and transcribe them with Gemini or OpenAI")
	parser.add_argument("video_paths", nargs="*", help="Paths to input video files")
	parser.add_argument("-l", "--video-list", dest="video_list_file", help="Text file containing one video path per line")
	parser.add_argument("-o", "--output-dir", dest="output_dir", help="Directory where transcripts are saved")
	parser.add_argument(
		"-m",
		"--model",
		dest="model",
		help="Model to transcribe with (default: %s)" % DEFAULT_TRANSCRIBE_MODEL,
	)
	args = parser.parse_args()

	load_dotenv()
	model = resolve_transcribe_model(args.model)
	provider = transcribe_model_provider(model)
	gemini_key = os.getenv("GEMINI_API_KEY")
	openai_key = os.getenv("OPENAI_API_KEY")
	if provider == "gemini" and not gemini_key:
		raise ValueError("Set GEMINI_API_KEY in your .env file")
	if provider == "openai" and not openai_key:
		raise ValueError("Set OPENAI_API_KEY in your .env file")

	video_paths = load_video_paths(args.video_paths, args.video_list_file)

	for video_path in video_paths:
		result = process_video(
			video_path,
			gemini_key,
			args.output_dir,
			model=model,
			openai_api_key=openai_key,
		)
		print(f"Saved transcript: {result['text_path']}")
		print(f"Saved captions: {result['srt_path']}")


if __name__ == "__main__":
	main()
