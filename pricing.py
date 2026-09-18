"""Time and cost estimates for a transcription run.

Two questions get answered here:

* *before* a run — "how long will this take and what will it cost?"
  (`estimate_run`, driven by the audio duration alone);
* *after* a run — "what did it actually cost?" (`report_run` for transcription,
  `report_llm_run` for the LLM proofreader, which reports real token counts).

Published prices move, so every number below can be overridden from the environment
with ``TRANSCRIBE_PRICING`` (JSON, keyed by model id) without touching this file.
Treat the defaults as an estimate for budgeting, not as a bill.
"""

from __future__ import annotations

import json
import logging
import os

logger = logging.getLogger("pricing")

# `per_minute` is USD per minute of audio, which is how the OpenAI speech-to-text
# models are quoted. `realtime_factor` is audio-seconds processed per wall-clock
# second under this pipeline's chunking and concurrency — it drives the time estimate
# and is deliberately conservative.
DEFAULT_PRICING: dict[str, dict] = {
	"gpt-transcribe": {
		"per_minute": 0.006,
		"realtime_factor": 14.0,
	},
	"gpt-4o-transcribe": {
		"per_minute": 0.006,
		"realtime_factor": 14.0,
	},
	"gpt-4o-mini-transcribe": {
		"per_minute": 0.003,
		"realtime_factor": 18.0,
	},
	"whisper-1": {
		"per_minute": 0.006,
		"realtime_factor": 22.0,
	},
}

# Chat models used by the optional LLM proofreader, priced in USD per 1M tokens.
LLM_PRICING: dict[str, dict] = {
	"gpt-5": {"input": 1.25, "output": 10.00},
	"gpt-5-mini": {"input": 0.25, "output": 2.00},
	"gpt-4o": {"input": 2.50, "output": 10.00},
	"gpt-4o-mini": {"input": 0.15, "output": 0.60},
}

# Used for a model we have no entry for, so an unknown id still produces a number
# rather than silently reporting zero.
FALLBACK_PRICING: dict = {"per_minute": 0.006, "realtime_factor": 12.0}
FALLBACK_LLM_PRICING: dict = {"input": 1.25, "output": 10.00}


def _env_overrides() -> dict[str, dict]:
	raw = os.getenv("TRANSCRIBE_PRICING", "").strip()
	if not raw:
		return {}
	try:
		parsed = json.loads(raw)
		if isinstance(parsed, dict):
			return parsed
	except json.JSONDecodeError:
		logger.warning("TRANSCRIBE_PRICING is not valid JSON; using built-in prices")
	return {}


def pricing_for(model: str) -> dict:
	"""Price entry for a model id, with environment overrides merged on top."""
	entry = dict(DEFAULT_PRICING.get(model, FALLBACK_PRICING))
	entry["is_estimate_only"] = model not in DEFAULT_PRICING
	override = _env_overrides().get(model)
	if isinstance(override, dict):
		entry.update(override)
	return entry


def _round_money(value: float) -> float:
	# Sub-cent runs are normal here, so keep enough places to stay meaningful.
	return round(value, 6)


def estimate_cost(model: str, audio_seconds: float) -> float:
	"""Projected USD for transcribing `audio_seconds` with `model`."""
	entry = pricing_for(model)
	per_minute = entry.get("per_minute", FALLBACK_PRICING["per_minute"])
	return _round_money(per_minute * max(audio_seconds, 0.0) / 60.0)


def estimate_seconds(model: str, audio_seconds: float) -> float:
	"""Projected wall-clock seconds for the run."""
	factor = pricing_for(model).get("realtime_factor") or FALLBACK_PRICING["realtime_factor"]
	# Floor covers the fixed overhead of audio extraction plus one API round trip.
	return round(max(audio_seconds / factor, 3.0), 1)


def estimate_run(model: str, provider: str, audio_seconds: float) -> dict:
	"""Pre-flight estimate, for showing the user the bill before they click."""
	return {
		"model": model,
		"provider": provider,
		"audio_seconds": round(audio_seconds, 2),
		"estimated_seconds": estimate_seconds(model, audio_seconds),
		"estimated_cost_usd": estimate_cost(model, audio_seconds),
		"basis": "per minute of audio",
		"is_estimate_only": pricing_for(model).get("is_estimate_only", False),
	}


def report_run(
	model: str,
	provider: str,
	audio_seconds: float,
	elapsed_seconds: float,
	usage: dict | None = None,
) -> dict:
	"""Post-run accounting for a transcription.

	Speech-to-text is billed per minute of audio, so duration is the basis; `usage`
	only contributes the request count.
	"""
	usage = usage or {}
	report = {
		"operation": "transcribe",
		"model": model,
		"provider": provider,
		"audio_seconds": round(max(audio_seconds, 0.0), 2),
		"elapsed_seconds": round(max(elapsed_seconds, 0.0), 2),
		"estimated_cost_usd": estimate_cost(model, audio_seconds),
		"cost_basis": "per minute of audio",
		"is_estimate_only": pricing_for(model).get("is_estimate_only", False),
		"requests": int(usage.get("requests") or 0),
	}
	if elapsed_seconds > 0 and audio_seconds > 0:
		report["realtime_factor"] = round(audio_seconds / elapsed_seconds, 1)
	return report


def report_llm_run(
	model: str,
	elapsed_seconds: float,
	input_tokens: int = 0,
	output_tokens: int = 0,
) -> dict:
	"""Post-run accounting for the LLM proofreader, from its reported token usage."""
	entry = dict(LLM_PRICING.get(model, FALLBACK_LLM_PRICING))
	override = _env_overrides().get(model)
	if isinstance(override, dict):
		entry.update(override)

	return {
		"operation": "rewrite",
		"model": model,
		"provider": "openai",
		"elapsed_seconds": round(max(elapsed_seconds, 0.0), 2),
		"input_tokens": int(input_tokens),
		"output_tokens": int(output_tokens),
		"estimated_cost_usd": _round_money(
			input_tokens * entry.get("input", 0.0) / 1_000_000
			+ output_tokens * entry.get("output", 0.0) / 1_000_000
		),
		"cost_basis": "reported token usage",
		"is_estimate_only": model not in LLM_PRICING,
	}
