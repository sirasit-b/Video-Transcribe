"""Turn raw ASR output into readable Thai subtitle lines.

Three jobs, in the order they have to happen:

1. `correct_segments` — run the word system (see `glossary.py`) over the text.
2. `split_segment` — cut long blocks into short lines (~42-52 chars) on real Thai
   word boundaries, and divide the block's time across those lines in proportion to
   how much text each one carries.
3. `sanitize_timings` — guarantee the result has no overlap and no zero-length cue.

Spacing repair (`normalize_spacing`) runs between 1 and 2, because replacing a Thai
transliteration with English is exactly what creates "เทรนmodel".
"""

from __future__ import annotations

import re

from glossary import THAI_CHAR_CLASS, Glossary, default_glossary

# Subtitle line budget. 42 is the low end we try to fill before considering a break;
# 52 is the hard ceiling (a two-line cue at this width stays readable on mobile).
MIN_LINE_CHARS = 42
MAX_LINE_CHARS = 52

# Below this, a trailing line looks like an orphan and gets folded back.
ORPHAN_LINE_CHARS = 16

# A cue shorter than this is unreadable, and a zero-length one breaks players.
MIN_CUE_SECONDS = 0.30

_THAI_RE = re.compile(f"[{THAI_CHAR_CLASS}]")

# Domain words the stock Thai dictionary does not know. Without these, newmm splits
# "เทรน" into "เท"+"รน" and a line break could land inside the word.
_EXTRA_DICT_WORDS: set[str] = {
	"เทรน", "เทรนนิ่ง", "เทรนนิง", "โมเดล", "โมดูล", "เซฟ", "พาธ", "แพทเทิร์น",
	"ดาต้าเซ็ต", "ดาต้าเซต", "เดต้าเซ็ต", "โน้ตบุ๊ก", "โน๊ตบุ๊ค", "เลเยอร์",
	"ไลบรารี่", "ไลบรารี", "เฟรมเวิร์ก", "ฟังก์ชัน", "ฟังก์ชั่น", "อัลกอริทึม",
	"อัลกอริธึม", "เพอร์เซปตรอน", "นิวรอลเน็ตเวิร์ก", "แมชชีนเลิร์นนิ่ง",
	"ออกเมนเทชั่น", "แอนโนเทชั่น", "บาวน์ดิ้งบ็อกซ์", "คอนฟิเดนซ์", "อีพ็อก",
	"แบตช์ไซส์", "เกรเดียนต์", "แบ็คพรอพาเกชั่น", "โอเวอร์ฟิตติ้ง", "เบราว์เซอร์",
	"ไดเรกทอรี", "รันไทม์", "เช็คพอยท์", "อิมพอร์ต", "โรโบโฟลว์", "โคแล็บ",
}

# Tokens that should start a line rather than end one: breaking just before a
# connector keeps a clause intact instead of stranding its first word.
_CONNECTORS: frozenset[str] = frozenset({
	"แล้ว", "แล้วก็", "และ", "แต่", "หรือ", "ก็", "ที่", "ซึ่ง", "เพราะ", "เพราะว่า",
	"ถ้า", "เมื่อ", "จาก", "ใน", "ของ", "กับ", "ให้", "จะ", "ว่า", "โดย", "เพื่อ",
	"ตอนนี้", "ต่อไป", "หลังจาก", "ก่อน", "ดังนั้น", "เดี๋ยว", "ทีนี้", "พอ",
})

# Tokens that end a thought: breaking right after one is the most natural cut there is.
_SENTENCE_ENDERS: frozenset[str] = frozenset({
	"ครับ", "ค่ะ", "คะ", "นะครับ", "นะคะ", "นะ", "ครับผม", "จ้า", "เลย", "แล้ว",
	"ด้วย", "กันนะ", "เนอะ", "ล่ะ", "ไหม", "มั้ย",
})

_PUNCT_ENDERS: frozenset[str] = frozenset({".", "!", "?", "。", "…"})


# ---------------------------------------------------------------------------
# Tokenisation
# ---------------------------------------------------------------------------
_custom_dict = None
_tokenizer_ready: bool | None = None


def _load_tokenizer() -> bool:
	"""Import pythainlp and build the custom dictionary once. False if unavailable."""
	global _custom_dict, _tokenizer_ready
	if _tokenizer_ready is not None:
		return _tokenizer_ready
	try:
		from pythainlp.corpus.common import thai_words
		from pythainlp.util import dict_trie

		_custom_dict = dict_trie(dict_source=set(thai_words()) | _EXTRA_DICT_WORDS)
		_tokenizer_ready = True
	except Exception:
		# The splitter still works without pythainlp, just with coarser boundaries.
		_custom_dict = None
		_tokenizer_ready = False
	return _tokenizer_ready


def _tokenize_thai(text: str) -> list[str]:
	if _load_tokenizer():
		from pythainlp.tokenize import word_tokenize

		return [
			token
			for token in word_tokenize(
				text, custom_dict=_custom_dict, engine="newmm", keep_whitespace=True
			)
			if token
		]
	# Fallback: break on Thai leading-vowel characters, which start a syllable. Much
	# coarser than newmm but still never cuts a Latin word or a number.
	return [part for part in re.split(r"(?=[เ-ไ])", text) if part]


def tokenize(text: str) -> list[str]:
	"""Atomic units a line break may fall between.

	Runs of non-Thai text (English words, file names, numbers, paths) are kept whole
	so a break can never land inside `best.pt` or `YOLOv8`; only the Thai runs go
	through the Thai word tokenizer.
	"""
	if not text:
		return []

	tokens: list[str] = []
	for chunk in re.split(f"([{THAI_CHAR_CLASS}]+)", text):
		if not chunk:
			continue
		if _THAI_RE.match(chunk):
			tokens.extend(_tokenize_thai(chunk))
		else:
			# Keep whitespace as its own token: it is the best break point we have.
			tokens.extend(part for part in re.split(r"(\s+)", chunk) if part)
	return tokens


# ---------------------------------------------------------------------------
# Spacing
# ---------------------------------------------------------------------------
def normalize_spacing(text: str) -> str:
	"""Re-separate Thai from Latin/digits and collapse redundant whitespace.

	Replacing "เทรนโมเดล" style transliterations with English leaves the two scripts
	glued together ("เทรนmodel"); this puts the space back.
	"""
	if not text:
		return ""

	cleaned = text.replace(" ", " ")
	cleaned = re.sub(f"([{THAI_CHAR_CLASS}])([A-Za-z0-9])", r"\1 \2", cleaned)
	cleaned = re.sub(f"([A-Za-z0-9])([{THAI_CHAR_CLASS}])", r"\1 \2", cleaned)
	# No space before closing punctuation we may have just introduced.
	cleaned = re.sub(r"\s+([,.!?;:)\]}])", r"\1", cleaned)
	cleaned = re.sub(r"([(\[{])\s+", r"\1", cleaned)
	cleaned = re.sub(r"[ \t]{2,}", " ", cleaned)
	return cleaned.strip()


# ---------------------------------------------------------------------------
# Line splitting
# ---------------------------------------------------------------------------
def _neighbours(tokens: list[str], cut: int) -> tuple[str, str]:
	"""The meaningful tokens either side of a cut, skipping whitespace tokens."""
	left = cut - 1
	while left >= 0 and not tokens[left].strip():
		left -= 1
	right = cut
	while right < len(tokens) and not tokens[right].strip():
		right += 1
	return (
		tokens[left].strip() if left >= 0 else "",
		tokens[right].strip() if right < len(tokens) else "",
	)


def _splits_latin_term(previous: str, following: str) -> bool:
	"""True when a cut here would fall inside a multi-word Latin term.

	"Working Directory" and "Machine Learning" read as single units, so breaking
	between their words is worse than breaking anywhere in the surrounding Thai —
	even though no individual word is cut.
	"""
	if not previous or not following:
		return False
	tail, head = previous[-1], following[0]
	return tail.isascii() and tail.isalnum() and head.isascii() and head.isalnum()


def _break_quality(tokens: list[str], cut: int) -> float:
	"""How natural a break between tokens[cut-1] and tokens[cut] is, ignoring length.

	Breaking after a full stop or a Thai sentence particle keeps a thought together;
	breaking just before a connector at least keeps the following clause intact.
	"""
	previous, following = _neighbours(tokens, cut)

	if previous in _PUNCT_ENDERS or previous.endswith(tuple(_PUNCT_ENDERS)):
		return 4000.0
	# Checked before the whitespace bonus: the space inside "Working Directory" is
	# exactly the break we want to avoid, and it would otherwise score well.
	if _splits_latin_term(previous, following):
		return -1500.0
	if previous in _SENTENCE_ENDERS:
		return 3000.0
	if not tokens[cut - 1].strip():  # the previous token *is* whitespace
		return 2000.0
	if following in _CONNECTORS:
		return 1000.0
	return 0.0


def _join(tokens: list[str]) -> str:
	# Thai is written without spaces, so tokens are concatenated as-is; any space in
	# the text is itself a token and survives the round trip.
	return "".join(tokens).strip()


def _split_tokens_into_lines(
	tokens: list[str],
	min_chars: int,
	max_chars: int,
) -> list[list[str]]:
	"""Greedy fill with a preference-ranked break point, never inside a token."""
	groups: list[list[str]] = []
	start = 0
	total = len(tokens)

	while start < total:
		# Skip leading whitespace tokens — they belong to the previous break.
		while start < total and not tokens[start].strip():
			start += 1
		if start >= total:
			break

		length = 0
		best_cut = -1
		best_score = float("-inf")
		hard_cut = -1  # last cut that still fits under max_chars
		cut = start

		while cut < total:
			length += len(tokens[cut])
			cut += 1
			if length > max_chars:
				break
			hard_cut = cut
			if length >= min_chars and cut < total:
				# Within the window a fuller line wins, so we never split text that
				# comfortably fits; break quality outranks fullness.
				score = _break_quality(tokens, cut) + length
				if score > best_score:
					best_score = score
					best_cut = cut

		if cut >= total and length <= max_chars:
			# Everything left fits on one line.
			groups.append(tokens[start:total])
			break

		# Prefer the best-scoring break inside the window; otherwise take the last
		# break that fits; if even one token exceeds max_chars, emit it alone rather
		# than cutting a word in half.
		chosen = best_cut if best_cut > start else hard_cut
		if chosen <= start:
			chosen = start + 1

		groups.append(tokens[start:chosen])
		start = chosen

	return [group for group in groups if _join(group)]


def _rebalance_tail(groups: list[list[str]], max_chars: int) -> list[list[str]]:
	"""Get rid of a stubby final line without ever exceeding the character ceiling.

	The last two lines are reconsidered together: merged if they fit on one line,
	otherwise re-split at the most balanced sensible word boundary. Merging with a
	length overshoot is deliberately not an option — the budget is the contract.
	"""
	if len(groups) < 2 or len(_join(groups[-1])) >= ORPHAN_LINE_CHARS:
		return groups

	combined = groups[-2] + groups[-1]
	if len(_join(combined)) <= max_chars:
		return groups[:-2] + [combined]

	best_cut = -1
	best_score = float("-inf")
	for cut in range(1, len(combined)):
		left = len(_join(combined[:cut]))
		right = len(_join(combined[cut:]))
		if not left or not right or left > max_chars or right > max_chars:
			continue
		# Balance is worth ~50 points per character of imbalance, so a genuinely
		# natural break still wins over a marginally more even one.
		score = _break_quality(combined, cut) - abs(left - right) * 50
		if score > best_score:
			best_score = score
			best_cut = cut

	if best_cut < 0:
		return groups
	return groups[:-2] + [combined[:best_cut], combined[best_cut:]]


def distribute_time(start: float, end: float, lines: list[str]) -> list[tuple[float, float]]:
	"""Divide [start, end] across `lines` in proportion to each line's length."""
	if not lines:
		return []
	if len(lines) == 1:
		return [(start, end)]

	span = max(end - start, 0.0)
	# Count non-space characters: spaces take no time to read.
	weights = [max(len(line.replace(" ", "")), 1) for line in lines]
	total_weight = sum(weights)

	# If the block is too short to give every line a readable slice, split it evenly
	# and let sanitize_timings deal with what is left.
	if span <= MIN_CUE_SECONDS * len(lines):
		step = span / len(lines) if span > 0 else 0.0
		return [(start + step * i, start + step * (i + 1)) for i in range(len(lines))]

	bounds: list[tuple[float, float]] = []
	cursor = start
	for index, weight in enumerate(weights):
		if index == len(weights) - 1:
			bounds.append((cursor, end))
			break
		duration = max(span * weight / total_weight, MIN_CUE_SECONDS)
		# Leave enough room for the remaining lines to clear the minimum.
		remaining = len(weights) - index - 1
		duration = min(duration, end - cursor - MIN_CUE_SECONDS * remaining)
		duration = max(duration, MIN_CUE_SECONDS)
		bounds.append((cursor, cursor + duration))
		cursor += duration
	return bounds


def split_segment(
	segment: dict,
	min_chars: int = MIN_LINE_CHARS,
	max_chars: int = MAX_LINE_CHARS,
) -> list[dict]:
	"""Split one caption block into short lines with proportionally divided time.

	A block already within the budget is returned untouched — the point is readable
	lines, not splitting for its own sake.
	"""
	text = (segment.get("text") or "").strip()
	start = float(segment.get("start", 0.0))
	end = float(segment.get("end", start))

	if not text:
		return []
	if len(text) <= max_chars:
		return [{"start": start, "end": end, "text": text}]

	groups = _rebalance_tail(_split_tokens_into_lines(tokenize(text), min_chars, max_chars), max_chars)
	lines = [_join(group) for group in groups if _join(group)]
	if not lines:
		return [{"start": start, "end": end, "text": text}]
	if len(lines) == 1:
		return [{"start": start, "end": end, "text": lines[0]}]

	return [
		{"start": line_start, "end": line_end, "text": line}
		for line, (line_start, line_end) in zip(lines, distribute_time(start, end, lines))
	]


def split_segments(
	segments: list[dict],
	min_chars: int = MIN_LINE_CHARS,
	max_chars: int = MAX_LINE_CHARS,
) -> list[dict]:
	result: list[dict] = []
	for segment in segments:
		result.extend(split_segment(segment, min_chars, max_chars))
	return result


# ---------------------------------------------------------------------------
# Timing repair
# ---------------------------------------------------------------------------
def sanitize_timings(
	segments: list[dict],
	min_duration: float = MIN_CUE_SECONDS,
) -> tuple[list[dict], list[dict]]:
	"""Remove overlaps and zero-length cues. Returns (segments, issues fixed).

	Input order is preserved rather than sorted by start time. ASR output is already
	chronological, so an overlap is a timing glitch — re-sorting on it would reorder
	the transcript's sentences to repair a few hundredths of a second.
	"""
	if not segments:
		return [], []

	ordered = [dict(segment) for segment in segments]
	issues: list[dict] = []
	fixed: list[dict] = []
	previous_end = 0.0

	for index, segment in enumerate(ordered):
		start = float(segment.get("start", 0.0))
		end = float(segment.get("end", start))
		text = (segment.get("text") or "").strip()
		if not text:
			continue

		original = (start, end)

		if start < previous_end:
			start = previous_end
		if start < 0:
			start = 0.0

		if end <= start:
			# Borrow room from the next cue when there is any, so a repaired cue does
			# not immediately create a fresh overlap.
			next_start = (
				float(ordered[index + 1].get("start", start + min_duration))
				if index + 1 < len(ordered)
				else start + min_duration
			)
			end = start + min_duration
			if next_start > start:
				end = min(end, max(next_start, start + min_duration))

		if (start, end) != original:
			issues.append(
				{
					"index": len(fixed),
					"kind": "overlap" if original[0] < previous_end else "zero_duration",
					"before": {"start": round(original[0], 3), "end": round(original[1], 3)},
					"after": {"start": round(start, 3), "end": round(end, 3)},
					"text": text,
				}
			)

		fixed.append({"start": round(start, 3), "end": round(end, 3), "text": text})
		previous_end = end

	return fixed, issues


# ---------------------------------------------------------------------------
# The full pass
# ---------------------------------------------------------------------------
def correct_segments(
	segments: list[dict],
	glossary: Glossary | None = None,
) -> tuple[list[dict], list[dict]]:
	"""Apply the word system to every block in one pass each, keeping timing intact."""
	glossary = glossary or default_glossary()
	corrected: list[dict] = []
	per_segment: list[dict] = []

	for index, segment in enumerate(segments):
		original = (segment.get("text") or "").strip()
		text, corrections = glossary.correct(original)
		text = normalize_spacing(text)
		corrected.append(
			{
				"start": float(segment.get("start", 0.0)),
				"end": float(segment.get("end", 0.0)),
				"text": text,
			}
		)
		if corrections or text != original:
			per_segment.append(
				{
					"index": index,
					"before": original,
					"after": text,
					"corrections": corrections,
				}
			)

	return corrected, per_segment


def polish_segments(
	segments: list[dict],
	glossary: Glossary | None = None,
	min_chars: int = MIN_LINE_CHARS,
	max_chars: int = MAX_LINE_CHARS,
	split_lines: bool = True,
) -> tuple[list[dict], dict]:
	"""Correct, re-space, split into lines, and repair timing — one call, one report.

	Returns the finished segments plus a report holding every highlighted correction,
	every timing repair, and enough counts for the UI to summarise the run.
	"""
	if not segments:
		return [], {
			"segments_in": 0,
			"segments_out": 0,
			"corrections": [],
			"correction_count": 0,
			"by_category": {},
			"by_rule": [],
			"timing_issues": [],
			"line_split_count": 0,
			"tokenizer": "pythainlp" if _load_tokenizer() else "fallback",
		}

	glossary = glossary or default_glossary()
	corrected, per_segment = correct_segments(segments, glossary)

	split = split_segments(corrected, min_chars, max_chars) if split_lines else corrected
	final, timing_issues = sanitize_timings(split)

	# Aggregate the highlights so the UI can show both "which words changed" and
	# "which rules are earning their keep".
	by_category: dict[str, int] = {}
	by_rule: dict[str, dict] = {}
	total = 0
	for entry in per_segment:
		for correction in entry["corrections"]:
			total += 1
			by_category[correction["category"]] = by_category.get(correction["category"], 0) + 1
			bucket = by_rule.setdefault(
				correction["rule_id"],
				{
					"rule_id": correction["rule_id"],
					"after": correction["after"],
					"category": correction["category"],
					"category_label": correction["category_label"],
					"count": 0,
					"variants": {},
				},
			)
			bucket["count"] += 1
			bucket["variants"][correction["before"]] = bucket["variants"].get(correction["before"], 0) + 1

	rule_summary = sorted(
		(
			{**bucket, "variants": sorted(bucket["variants"].items(), key=lambda item: -item[1])}
			for bucket in by_rule.values()
		),
		key=lambda item: -item["count"],
	)

	report = {
		"segments_in": len(segments),
		"segments_out": len(final),
		"corrections": per_segment,
		"correction_count": total,
		"by_category": by_category,
		"by_rule": rule_summary,
		"timing_issues": timing_issues,
		"line_split_count": max(len(split) - len(corrected), 0),
		"tokenizer": "pythainlp" if _load_tokenizer() else "fallback",
		"line_budget": {"min": min_chars, "max": max_chars},
		"glossary": glossary.stats(),
	}
	return final, report
