"""auto-editor's own timeline JSON for an auto-trimmed edit.

The XML exports go to an editor; this one goes back to auto-editor, or to any
script that wants the cut list as data. `auto-editor timeline.json -o out.mp4`
re-renders from it, so a trim decided here can be re-run, tweaked or diffed
elsewhere without re-analyzing anything.

Two shapes, both of which auto-editor imports (`src/imports/json.nim`):

* **v3** is the full timeline — resolution, sample rate, layout, and one clip per
  kept range on a video layer and an audio layer per stream. This is what
  `--export json` writes.
* **v1** is the compact cut list: every chunk of the timeline in order, kept ones
  at speed 1 and cut ones at 99999 (auto-editor's "remove this" speed). Nothing
  is left implicit, which makes it the easy one to read from a script.

Unlike the XML exports, `src` here is a plain filesystem path rather than a
`file://` URL, because that is what auto-editor opens.
"""

from __future__ import annotations

import json
import re
from pathlib import PurePosixPath

# auto-editor's sentinel speed for "this chunk is not in the output".
CUT_SPEED = 99999.0

_LAYOUTS = {1: "mono", 2: "stereo", 6: "5.1", 8: "7.1"}


def _source_path(original_name: str, media_path: str | None) -> str:
    """Where the footage will be when the JSON is used.

    auto-editor opens this path to read the media, so a bare filename works only
    if it runs beside the file. `media_path` is the folder it will actually sit in.
    """
    name = PurePosixPath(original_name.replace("\\", "/")).name or "source.mp4"
    if media_path:
        return f"{media_path.replace(chr(92), '/').rstrip('/')}/{name}"
    return name


def _project_name(original_name: str) -> str:
    stem = PurePosixPath(original_name.replace("\\", "/")).name
    return re.sub(r"\.[^.]+$", "", stem) or "Auto trim"


def _kept(segments: list[dict]) -> list[tuple[int, int]]:
    return [
        (int(s["start_frame"]), int(s["end_frame"]))
        for s in segments
        if int(s["end_frame"]) > int(s["start_frame"])
    ]


def build_timeline_json(
    *,
    original_name: str,
    segments: list[dict],
    timebase_num: int,
    timebase_den: int,
    width: int,
    height: int,
    total_frames: int,
    sample_rate: int = 48000,
    channels: int = 2,
    audio_streams: int = 1,
    has_video: bool = True,
    version: str = "3",
    media_path: str | None = None,
) -> str:
    """One edit as an auto-editor timeline, as pretty-printed JSON."""
    if version not in ("1", "3"):
        raise ValueError(f"Unsupported timeline version: {version}")
    if timebase_num <= 0 or timebase_den <= 0:
        raise ValueError("timebase must be positive")

    timebase = f"{timebase_num}/{timebase_den}"
    source = _source_path(original_name, media_path)
    kept = _kept(segments)

    if version == "1":
        # Every frame of the timeline is accounted for: the gaps between kept
        # ranges are chunks too, at the speed that means "drop this".
        chunks: list[list[float | int]] = []
        cursor = 0
        for start, end in kept:
            if start > cursor:
                chunks.append([cursor, start, CUT_SPEED])
            chunks.append([start, end, 1.0])
            cursor = end
        if total_frames > cursor:
            chunks.append([cursor, total_frames, CUT_SPEED])

        return json.dumps(
            {
                "version": "1",
                "source": source,
                "timebase": timebase,
                "chunks": chunks,
            },
            indent=2,
            ensure_ascii=False,
        )

    # v3. The importer insists on even dimensions of at least 2, and on layers
    # that are not empty — a timeline that keeps nothing has nothing to export.
    if not kept:
        raise ValueError("this edit keeps nothing, so there is no timeline to write")

    def even(value: int, fallback: int) -> int:
        value = value if value >= 2 else fallback
        return value - (value % 2)

    def clips(stream: int) -> list[dict]:
        """One clip per kept range: where it lands, and where it came from."""
        timeline = 0
        out = []
        for start, end in kept:
            out.append(
                {
                    "src": source,
                    "start": timeline,  # frame on the timeline
                    "dur": end - start,
                    "offset": start,  # frame in the source
                    "stream": stream,
                }
            )
            timeline += end - start
        return out

    layers_v = [clips(0)] if has_video else []
    layers_a = [clips(stream) for stream in range(max(1, audio_streams))]

    return json.dumps(
        {
            "version": "3",
            "templateFile": source,
            "timebase": timebase,
            "background": "#000000",
            "resolution": [even(width, 1920), even(height, 1080)],
            "samplerate": max(100, sample_rate),
            "layout": _LAYOUTS.get(channels, "stereo"),
            "langs": ["und"] * (len(layers_v) + len(layers_a)),
            "v": layers_v,
            "a": layers_a,
            # Not part of what auto-editor reads; it is here so a person opening
            # the file can see which edit it describes.
            "auto_trim": {
                "name": _project_name(original_name),
                "kept_frames": sum(end - start for start, end in kept),
                "total_frames": total_frames,
                "segment_count": len(kept),
            },
        },
        indent=2,
        ensure_ascii=False,
    )
