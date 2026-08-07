import argparse
import json
import os
import re
import shutil
import subprocess
import tempfile
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

from openai import OpenAI
from dotenv import load_dotenv


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

	# Whisper only uses 16kHz mono, so downmix + downsample to shrink the upload payload.
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


def _transcribe_chunk(client: OpenAI, audio_path: str) -> tuple[str, list[dict]]:
	with open(audio_path, "rb") as audio_file:
		transcription = client.audio.transcriptions.create(
			model="whisper-1",
			language="th",
			file=audio_file,
			response_format="verbose_json",
			timestamp_granularities=["segment"],
		)

	segments = [
		{"start": float(seg.start), "end": float(seg.end), "text": seg.text.strip()}
		for seg in transcription.segments
	]
	return transcription.text, segments


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


def transcribe_audio_file(
	audio_path: str,
	api_key: str,
	chunk_seconds: int = 120,
	max_workers: int = 6,
) -> dict:
	client = OpenAI(api_key=api_key)

	try:
		duration = get_video_duration_seconds(audio_path)
	except Exception:
		duration = 0.0

	# Short clips: one request avoids split/merge overhead.
	if duration <= chunk_seconds * 1.5:
		text, segments = _transcribe_chunk(client, audio_path)
		return {"text": text, "segments": segments}

	# Long clips: split and transcribe chunks concurrently to parallelize API latency.
	with tempfile.TemporaryDirectory() as chunk_dir:
		chunks = _split_audio_into_chunks(audio_path, chunk_seconds, chunk_dir)
		if len(chunks) <= 1:
			text, segments = _transcribe_chunk(client, audio_path)
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
				executor.submit(_transcribe_chunk, client, str(chunk)): idx
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


def rewrite_caption_segments(segments: list[dict], api_key: str) -> list[dict]:
	"""Fix typos/spelling in caption text using an LLM while keeping timing unchanged."""
	if not segments:
		return []

	client = OpenAI(api_key=api_key)
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


def process_video(video_path: str, api_key: str, output_dir: str | None = None) -> dict:
	video_file = Path(video_path)
	if not video_file.exists():
		raise FileNotFoundError(f"Input file not found: {video_file}")

	output_base = Path(output_dir) if output_dir else video_file.parent
	output_base.mkdir(parents=True, exist_ok=True)

	with tempfile.TemporaryDirectory() as temp_dir:
		audio_path = Path(temp_dir) / f"{video_file.stem}.mp3"
		extract_audio_to_mp3(str(video_file), str(audio_path))
		result = transcribe_audio_file(str(audio_path), api_key)

	text_path = output_base / f"{video_file.stem}.txt"
	srt_path = output_base / f"{video_file.stem}.srt"
	text_path.write_text(result["text"], encoding="utf-8")
	srt_path.write_text(segments_to_srt(result["segments"]), encoding="utf-8")

	return {
		"text": result["text"],
		"segments": result["segments"],
		"text_path": text_path,
		"srt_path": srt_path,
	}


def main() -> None:
	parser = argparse.ArgumentParser(description="Extract audio from video files and transcribe them with OpenAI")
	parser.add_argument("video_paths", nargs="*", help="Paths to input video files")
	parser.add_argument("-l", "--video-list", dest="video_list_file", help="Text file containing one video path per line")
	parser.add_argument("-o", "--output-dir", dest="output_dir", help="Directory where transcripts are saved")
	args = parser.parse_args()

	load_dotenv()
	api_key = os.getenv("OPENAI_API_KEY")
	if not api_key:
		raise ValueError("Set OPENAI_API_KEY in your .env file")

	video_paths = load_video_paths(args.video_paths, args.video_list_file)

	for video_path in video_paths:
		result = process_video(video_path, api_key, args.output_dir)
		print(f"Saved transcript: {result['text_path']}")
		print(f"Saved captions: {result['srt_path']}")


if __name__ == "__main__":
	main()
