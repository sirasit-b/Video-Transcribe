"""Final Cut Pro XML for an auto-trimmed edit.

The XML references the **original** upload, not the trimmed render: Final Cut opens
a project whose cuts are already made but still adjustable, which is the point of
taking an edit into an editor at all. Each kept range becomes one `asset-clip` on
the spine, carrying the source's picture and audio together.

Times in FCPXML are rationals in seconds with the timebase as the denominator, so a
frame count `n` at 30000/1001 is written `{n * 1001}/30000s`. Writing them as
decimals is what makes an imported timeline drift; keeping the source's own
denominator means every cut lands exactly on the frame the edit chose.

Shaped after auto-editor's `src/exports/fcp11.nim`, which is the reference for what
Final Cut actually accepts.
"""

from __future__ import annotations

import re
from pathlib import PurePosixPath
from urllib.parse import quote
from xml.sax.saxutils import quoteattr

# FCPXML versions Final Cut reads: 1.11 is 10.6.8 and later, 1.10 a little older.
_VERSIONS = {"11": "1.11", "10": "1.10"}

# A sequence only accepts these two layouts; anything wider is "surround".
def _audio_layout(channels: int) -> str:
    return "surround" if channels > 2 else "stereo"


def _audio_rate(sample_rate: int) -> str:
    return "44.1k" if sample_rate == 44100 else "48k"


def _format_name(width: int, height: int, fps: float) -> str:
    """Final Cut's own name for a few common formats, else the neutral one.

    An unrecognised name is not an error — `FFVideoFormatRateUndefined` tells Final
    Cut to take the frame duration and size from the attributes instead.
    """
    if height == 720 and abs(fps - 30) < 0.01:
        return "FFVideoFormat720p30"
    if height == 720 and abs(fps - 25) < 0.01:
        return "FFVideoFormat720p25"
    if (width, height) == (3840, 2160) and abs(fps - 24000 / 1001) < 0.01:
        return "FFVideoFormat3840x2160p2398"
    return "FFVideoFormatRateUndefined"


def _media_uri(original_name: str, media_path: str | None) -> str:
    """A `file://` URL for the source.

    Final Cut relinks media it cannot find, so a bare filename still imports — it
    just asks where the footage lives. `media_path` (the folder the footage sits in
    on the editing machine) spares that step.
    """
    name = PurePosixPath(original_name.replace("\\", "/")).name or "source.mp4"
    if media_path:
        folder = media_path.replace("\\", "/").rstrip("/")
        path = f"{folder}/{name}"
    else:
        path = f"/{name}"
    if not path.startswith("/"):
        path = f"/{path}"
    # Keep the separators; encode everything a URL cannot carry (spaces, Thai, ...).
    return "file://" + quote(path, safe="/-._~")


def _project_name(original_name: str) -> str:
    stem = PurePosixPath(original_name.replace("\\", "/")).name
    stem = re.sub(r"\.[^.]+$", "", stem)
    return stem or "Auto trim"


def build_fcpxml(
    *,
    original_name: str,
    segments: list[dict],
    timebase_num: int,
    timebase_den: int,
    width: int,
    height: int,
    source_duration: float,
    sample_rate: int = 48000,
    channels: int = 2,
    has_video: bool = True,
    version: str = "11",
    media_path: str | None = None,
    event_name: str = "Auto trim",
) -> str:
    """Render one edit as an FCPXML document.

    `segments` are the kept ranges as the trim service reports them, each with
    `start_frame` and `end_frame` on the timeline's own timebase.
    """
    if version not in _VERSIONS:
        raise ValueError(f"Unsupported Final Cut Pro XML version: {version}")
    if timebase_num <= 0 or timebase_den <= 0:
        raise ValueError("timebase must be positive")

    fps = timebase_num / timebase_den

    def fraction(frames: int) -> str:
        # "0s" rather than "0/30s": Final Cut writes it that way, and some
        # importers are fussy about a zero numerator.
        if frames == 0:
            return "0s"
        return f"{frames * timebase_den}/{timebase_num}s"

    # An asset's duration is the source's, not the shorter edited timeline: the
    # clips point *into* it.
    source_frames = max(1, round(source_duration * fps))
    project = _project_name(original_name)

    lines: list[str] = [
        "<?xml version='1.0' encoding='utf-8'?>",
        "<!DOCTYPE fcpxml>",
        f"<fcpxml version={quoteattr(_VERSIONS[version])}>",
        "  <resources>",
        "    <format id=\"r1\""
        f" name={quoteattr(_format_name(width, height, fps))}"
        f" frameDuration={quoteattr(fraction(1))}"
        f" width={quoteattr(str(width))}"
        f" height={quoteattr(str(height))}"
        " colorSpace=\"1-1-1 (Rec. 709)\"/>",
        "    <asset id=\"r2\""
        f" name={quoteattr(project)}"
        " start=\"0s\""
        f" hasVideo={quoteattr('1' if has_video else '0')}"
        " format=\"r1\""
        " hasAudio=\"1\""
        " audioSources=\"1\""
        f" audioChannels={quoteattr(str(max(1, channels)))}"
        f" duration={quoteattr(fraction(source_frames))}>",
        f"      <media-rep kind=\"original-media\" src={quoteattr(_media_uri(original_name, media_path))}/>",
        "    </asset>",
        "  </resources>",
        "  <library>",
        f"    <event name={quoteattr(event_name)}>",
        f"      <project name={quoteattr(project)}>",
        "        <sequence format=\"r1\" tcStart=\"0s\" tcFormat=\"NDF\""
        f" audioLayout={quoteattr(_audio_layout(channels))}"
        f" audioRate={quoteattr(_audio_rate(sample_rate))}>",
        "          <spine>",
    ]

    # `offset` is where the clip sits on the timeline, `start` where it begins in
    # the source. Laying the clips end to end is what closes the gaps.
    timeline = 0
    for segment in segments:
        start = int(segment["start_frame"])
        end = int(segment["end_frame"])
        duration = end - start
        if duration <= 0:
            continue
        lines.append(
            "            <asset-clip"
            f" name={quoteattr(project)}"
            " ref=\"r2\""
            f" offset={quoteattr(fraction(timeline))}"
            f" duration={quoteattr(fraction(duration))}"
            f" start={quoteattr(fraction(start))}"
            " tcFormat=\"NDF\"/>"
        )
        timeline += duration

    lines += [
        "          </spine>",
        "        </sequence>",
        "      </project>",
        "    </event>",
        "  </library>",
        "</fcpxml>",
        "",
    ]
    return "\n".join(lines)
