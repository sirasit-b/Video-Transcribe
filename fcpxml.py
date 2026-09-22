"""Editor projects for an auto-trimmed edit, in the two XMLs editors read.

`build_fcpxml` writes **FCPXML** (`.fcpxml`), which Final Cut Pro 10.6.8 and later
import. `build_xmeml` writes the older **Final Cut Pro 7 interchange XML**
(`.xml`), which is what Premiere Pro, DaVinci Resolve and Final Cut Pro 7 read —
different document, same edit.

Either way the XML references the **original** upload, not the trimmed render: the
editor opens a project whose cuts are already made but still adjustable, which is
the point of taking an edit into an editor at all. Each kept range becomes one clip
carrying the source's picture and audio together.

Times in FCPXML are rationals in seconds with the timebase as the denominator, so a
frame count `n` at 30000/1001 is written `{n * 1001}/30000s`. Writing them as
decimals is what makes an imported timeline drift; keeping the source's own
denominator means every cut lands exactly on the frame the edit chose.

Shaped after auto-editor's `src/exports/fcp11.nim`, which is the reference for what
Final Cut actually accepts.
"""

from __future__ import annotations

import re
from fractions import Fraction
from math import ceil
from pathlib import PurePosixPath
from urllib.parse import quote
from xml.sax.saxutils import escape, quoteattr

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


# ---------------------------------------------------------------------------
# Final Cut Pro 7 interchange XML (xmeml) — what Premiere, Resolve and FCP 7 read
# ---------------------------------------------------------------------------
# This format counts in whole frames on a *rounded* timebase with an NTSC flag,
# rather than in rationals: 29.97 is "timebase 30, ntsc TRUE". Frame numbers are
# exact either way, which is what matters — the flag tells the editor how long a
# frame is.


def _timebase_and_ntsc(timebase_num: int, timebase_den: int) -> tuple[int, str]:
    """The (timebase, ntsc) pair this rate is written as.

    Apple's table: https://developer.apple.com/library/archive/documentation/AppleApplications/Reference/FinalCutPro_XML/FrameRate/FrameRate.html
    """
    exact = (timebase_num, timebase_den)
    if exact == (24000, 1001):
        return 24, "TRUE"
    if exact == (30000, 1001):
        return 30, "TRUE"
    if exact == (60000, 1001):
        return 60, "TRUE"

    fps = timebase_num / timebase_den
    rounded = ceil(fps)
    # Any other 1000/1001-pulled rate (25000/1001, say) is still an NTSC rate.
    if Fraction(timebase_num, timebase_den) == Fraction(rounded) * Fraction(1000, 1001):
        return rounded, "TRUE"

    # Last resort for a rate that is neither whole nor NTSC: truncate. The frame
    # numbers stay right; only the declared frame length is approximate.
    return max(1, timebase_num // timebase_den), "FALSE"


def _rate(timebase: int, ntsc: str, indent: str) -> list[str]:
    return [
        f"{indent}<rate>",
        f"{indent}  <timebase>{timebase}</timebase>",
        f"{indent}  <ntsc>{ntsc}</ntsc>",
        f"{indent}</rate>",
    ]


def build_xmeml(
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
    media_path: str | None = None,
    sequence_name: str | None = None,
) -> str:
    """Render one edit as a Final Cut Pro 7 interchange XML document.

    `segments` are the kept ranges as the trim service reports them. A clip's
    `start`/`end` are where it sits on the timeline and `in`/`out` where it comes
    from in the source, all in frames.
    """
    if timebase_num <= 0 or timebase_den <= 0:
        raise ValueError("timebase must be positive")

    timebase, ntsc = _timebase_and_ntsc(timebase_num, timebase_den)
    fps = timebase_num / timebase_den
    name = sequence_name or _project_name(original_name)
    url = _media_uri(original_name, media_path)
    stereo = channels >= 2
    # Premiere explodes a stereo source into one track per channel.
    audio_tracks = 2 if stereo else 1

    kept = [
        (int(s["start_frame"]), int(s["end_frame"]))
        for s in segments
        if int(s["end_frame"]) > int(s["start_frame"])
    ]
    timeline_frames = sum(end - start for start, end in kept)

    def clip_times(index: int) -> tuple[int, int, int, int]:
        """(start, end, in, out) for the clip at `index`, in frames."""
        before = sum(end - start for start, end in kept[:index])
        start, end = kept[index]
        return before, before + (end - start), start, end

    # Ids: the video clips first, then one block per audio track, so a <link> can
    # name any of them without ambiguity.
    def video_id(index: int) -> str:
        return f"clipitem-{index + 1}"

    def audio_id(track: int, index: int) -> str:
        return f"clipitem-{len(kept) * (track + 1) + index + 1}"

    def links(index: int, indent: str) -> list[str]:
        """What ties a clip to its own audio, so they move together in the editor."""
        out = []
        for ref, media_type, track_index in [
            (video_id(index), "video", 1),
            *[(audio_id(track, index), "audio", track + 1) for track in range(audio_tracks)],
        ]:
            out += [
                f"{indent}<link>",
                f"{indent}  <linkclipref>{ref}</linkclipref>",
                f"{indent}  <mediatype>{media_type}</mediatype>",
                f"{indent}  <trackindex>{track_index}</trackindex>",
                f"{indent}  <clipindex>{index + 1}</clipindex>",
                f"{indent}</link>",
            ]
        return out

    # The file is defined once, in full, and referenced by id after that — the
    # convention every reader of this format expects.
    written_file = {"done": False}

    def file_element(indent: str) -> list[str]:
        if written_file["done"]:
            return [f'{indent}<file id="file-1"/>']
        written_file["done"] = True
        out = [
            f'{indent}<file id="file-1">',
            f"{indent}  <name>{escape(PurePosixPath(original_name.replace(chr(92), '/')).name)}</name>",
            f"{indent}  <pathurl>{escape(url)}</pathurl>",
            f"{indent}  <timecode>",
            f"{indent}    <string>00:00:00:00</string>",
            f"{indent}    <displayformat>NDF</displayformat>",
            *_rate(timebase, ntsc, f"{indent}    "),
            f"{indent}  </timecode>",
            *_rate(timebase, ntsc, f"{indent}  "),
            # Left empty on purpose: the editor reads the real length from the
            # media, and a wrong number here is worse than none.
            f"{indent}  <duration></duration>",
            f"{indent}  <media>",
        ]
        if has_video:
            out += [
                f"{indent}    <video>",
                f"{indent}      <samplecharacteristics>",
                *_rate(timebase, ntsc, f"{indent}        "),
                f"{indent}        <width>{width}</width>",
                f"{indent}        <height>{height}</height>",
                f"{indent}        <pixelaspectratio>square</pixelaspectratio>",
                f"{indent}      </samplecharacteristics>",
                f"{indent}    </video>",
            ]
        out += [
            f"{indent}    <audio>",
            f"{indent}      <samplecharacteristics>",
            f"{indent}        <depth>16</depth>",
            f"{indent}        <samplerate>{sample_rate}</samplerate>",
            f"{indent}      </samplecharacteristics>",
            f"{indent}      <channelcount>{max(1, channels)}</channelcount>",
            f"{indent}    </audio>",
            f"{indent}  </media>",
            f"{indent}</file>",
        ]
        return out

    lines: list[str] = [
        "<?xml version='1.0' encoding='utf-8'?>",
        "<!DOCTYPE xmeml>",
        '<xmeml version="5">',
        '  <sequence explodedTracks="true">',
        f"    <name>{escape(name)}</name>",
        f"    <duration>{timeline_frames}</duration>",
        *_rate(timebase, ntsc, "    "),
        "    <media>",
        "      <video>",
        "        <format>",
        "          <samplecharacteristics>",
        f"            <width>{width}</width>",
        f"            <height>{height}</height>",
        "            <pixelaspectratio>square</pixelaspectratio>",
        *_rate(timebase, ntsc, "            "),
        "          </samplecharacteristics>",
        "        </format>",
        "        <track>",
    ]

    for index in range(len(kept)):
        start, end, source_in, source_out = clip_times(index)
        lines += [
            f'          <clipitem id="{video_id(index)}">',
            f"            <name>{escape(name)}</name>",
            "            <enabled>TRUE</enabled>",
            f"            <start>{start}</start>",
            f"            <end>{end}</end>",
            f"            <in>{source_in}</in>",
            f"            <out>{source_out}</out>",
            *file_element("            "),
            "            <compositemode>normal</compositemode>",
            *links(index, "            "),
            "          </clipitem>",
        ]

    lines += [
        "        </track>",
        "      </video>",
        "      <audio>",
        f"        <numOutputChannels>{2 if stereo else 1}</numOutputChannels>",
        "        <format>",
        "          <samplecharacteristics>",
        "            <depth>16</depth>",
        f"            <samplerate>{sample_rate}</samplerate>",
        "          </samplecharacteristics>",
        "        </format>",
    ]

    for track in range(audio_tracks):
        lines += [
            f'        <track currentExplodedTrackIndex="{track}"'
            f' totalExplodedTrackCount="{audio_tracks}"'
            f' premiereTrackType="{"Stereo" if stereo else "Mono"}">'
        ]
        if stereo:
            lines.append(f"          <outputchannelindex>{track + 1}</outputchannelindex>")
        for index in range(len(kept)):
            start, end, source_in, source_out = clip_times(index)
            lines += [
                f'          <clipitem id="{audio_id(track, index)}"'
                f' premiereChannelType="{"stereo" if stereo else "mono"}">',
                f"            <name>{escape(name)}</name>",
                "            <enabled>TRUE</enabled>",
                f"            <start>{start}</start>",
                f"            <end>{end}</end>",
                f"            <in>{source_in}</in>",
                f"            <out>{source_out}</out>",
                *file_element("            "),
                "            <sourcetrack>",
                "              <mediatype>audio</mediatype>",
                f"              <trackindex>{track + 1}</trackindex>",
                "            </sourcetrack>",
                *links(index, "            "),
                "          </clipitem>",
            ]
        lines.append("        </track>")

    lines += [
        "      </audio>",
        "    </media>",
        "  </sequence>",
        "</xmeml>",
        "",
    ]
    _ = (source_duration, fps)  # The source length is the editor's to discover.
    return "\n".join(lines)
