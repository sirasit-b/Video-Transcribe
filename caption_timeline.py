"""Moving caption timings onto a cut timeline.

A transcript is made from the recording as it was shot; the cut is the same
recording with its silences taken out. Every cue after the first cut therefore
sits later in the transcript than it does in the file someone is actually
watching, and by the end of a ten-minute take the two are minutes apart. Playing
the cut against the original timings is not slightly wrong, it is unusable.

So the cues are moved: each time on the source timeline becomes the time it has in
the cut, which is simply how much kept material comes before it. A cue that fell
entirely inside a silence has nowhere to go and is dropped — it was the silence
that was cut, so there was nothing being said in it anyway.
"""

from typing import Iterable, Optional


def _ranges(kept: Iterable[dict]) -> list[tuple[float, float]]:
    """The kept ranges as (start, end) seconds, in order and without the empty ones."""
    ranges = []
    for segment in kept or []:
        try:
            start = float(segment["start"])
            end = float(segment["end"])
        except (KeyError, TypeError, ValueError):
            continue
        if end > start:
            ranges.append((start, end))
    ranges.sort()
    return ranges


def to_cut_time(seconds: float, ranges: list[tuple[float, float]], *, forward: bool) -> Optional[float]:
    """Where `seconds` on the source timeline lands in the cut.

    `forward` says what to do with a time that falls inside a piece that was cut
    out: a cue's start moves to the beginning of the next kept range (it appears
    as soon as there is picture again), and a cue's end moves back to the end of
    the previous one. Taken the other way round, a cue that straddles a cut would
    grow to cover material it has nothing to do with.

    Returns None only when the time is past everything that was kept.
    """
    elapsed = 0.0
    for start, end in ranges:
        if seconds < start:
            # In the gap before this range.
            return elapsed if forward else elapsed
        if seconds < end:
            return elapsed + (seconds - start)
        elapsed += end - start
    return None if forward else elapsed


def shift_captions(captions: Iterable[dict], kept: Iterable[dict]) -> list[dict]:
    """The same cues, timed against the cut instead of the original.

    With no cut to shift onto, the cues are returned as they are: a transcript of
    an untrimmed recording is already on the only timeline there is.
    """
    captions = list(captions or [])
    ranges = _ranges(kept)
    if not ranges:
        return captions

    shifted = []
    for cue in captions:
        try:
            start = float(cue.get("start", 0.0))
            end = float(cue.get("end", 0.0))
        except (TypeError, ValueError):
            continue

        cut_start = to_cut_time(start, ranges, forward=True)
        cut_end = to_cut_time(end, ranges, forward=False)
        if cut_start is None or cut_end is None or cut_end <= cut_start:
            # The whole cue was inside something that was cut out.
            continue
        shifted.append({**cue, "start": round(cut_start, 3), "end": round(cut_end, 3)})
    return shifted
