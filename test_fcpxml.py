"""Checks on the Final Cut Pro XML export.

The things that matter are the ones that go wrong silently: a frame count written
as a decimal, clips that do not sit end to end, a filename that breaks the XML.
"""

import xml.etree.ElementTree as ET
from fractions import Fraction

import fcpxml

SEGMENTS = [
    {"start_frame": 0, "end_frame": 24},
    {"start_frame": 34, "end_frame": 395},
    {"start_frame": 410, "end_frame": 522},
]


def build(**overrides) -> str:
    kwargs = dict(
        original_name="lecture 01.mp4",
        segments=SEGMENTS,
        timebase_num=30,
        timebase_den=1,
        width=1280,
        height=720,
        source_duration=42.4533,
        sample_rate=48000,
        channels=2,
    )
    kwargs.update(overrides)
    return fcpxml.build_fcpxml(**kwargs)


def seconds(value: str) -> Fraction:
    """Parse an FCPXML time, which is a rational number of seconds."""
    assert value.endswith("s"), value
    body = value[:-1]
    return Fraction(body) if "/" in body else Fraction(body)


def test_document_parses_and_has_the_expected_shape():
    root = ET.fromstring(build())
    assert root.tag == "fcpxml"
    assert root.attrib["version"] == "1.11"
    assert root.find("./resources/format") is not None
    asset = root.find("./resources/asset")
    assert asset.attrib["hasVideo"] == "1"
    assert asset.attrib["hasAudio"] == "1"
    assert root.find("./library/event/project/sequence/spine") is not None


def test_clips_sit_end_to_end_with_the_source_offsets():
    root = ET.fromstring(build())
    clips = root.findall("./library/event/project/sequence/spine/asset-clip")
    assert len(clips) == len(SEGMENTS)

    expected_offset = Fraction(0)
    for clip, segment in zip(clips, SEGMENTS):
        frames = segment["end_frame"] - segment["start_frame"]
        assert seconds(clip.attrib["duration"]) == Fraction(frames, 30)
        # Where it sits on the timeline: right after the clip before it, which is
        # what removes the silence.
        assert seconds(clip.attrib["offset"]) == expected_offset
        # Where it comes from in the source: the frame the edit chose.
        assert seconds(clip.attrib["start"]) == Fraction(segment["start_frame"], 30)
        expected_offset += Fraction(frames, 30)

    kept_frames = sum(s["end_frame"] - s["start_frame"] for s in SEGMENTS)
    assert expected_offset == Fraction(kept_frames, 30)


def test_ntsc_rates_keep_their_denominator():
    # 29.97 written as a decimal is what makes an imported timeline drift; the
    # source's own denominator keeps every cut on its frame.
    xml = build(timebase_num=30000, timebase_den=1001)
    root = ET.fromstring(xml)
    fmt = root.find("./resources/format")
    assert fmt.attrib["frameDuration"] == "1001/30000s"

    clip = root.find("./library/event/project/sequence/spine/asset-clip")
    assert clip.attrib["duration"] == "24024/30000s"
    assert seconds(clip.attrib["duration"]) == Fraction(24 * 1001, 30000)


def test_zero_is_written_the_way_final_cut_writes_it():
    root = ET.fromstring(build())
    clip = root.find("./library/event/project/sequence/spine/asset-clip")
    assert clip.attrib["offset"] == "0s"
    assert clip.attrib["start"] == "0s"


def test_the_asset_is_as_long_as_the_source_not_the_edit():
    root = ET.fromstring(build())
    asset = root.find("./resources/asset")
    # 42.4533s at 30fps rounds to 1274 frames; the edit keeps far fewer.
    assert asset.attrib["duration"] == "1274/30s"


def test_media_path_makes_an_absolute_file_url():
    root = ET.fromstring(build(media_path="/Volumes/Work/footage"))
    src = root.find("./resources/asset/media-rep").attrib["src"]
    assert src == "file:///Volumes/Work/footage/lecture%2001.mp4"


def test_without_a_media_path_the_bare_name_is_used():
    root = ET.fromstring(build())
    src = root.find("./resources/asset/media-rep").attrib["src"]
    assert src == "file:///lecture%2001.mp4"


def test_a_thai_filename_survives_as_a_url_and_as_xml():
    xml = build(original_name="บทเรียน & เสียง.mp4")
    root = ET.fromstring(xml)  # would raise if the name broke the document
    src = root.find("./resources/asset/media-rep").attrib["src"]
    assert src.startswith("file:///%E0%B8%9A")
    assert root.find("./library/event/project").attrib["name"] == "บทเรียน & เสียง"


def test_audio_layout_follows_the_channel_count():
    stereo = ET.fromstring(build(channels=2))
    assert stereo.find("./library/event/project/sequence").attrib["audioLayout"] == "stereo"
    mono = ET.fromstring(build(channels=1))
    assert mono.find("./library/event/project/sequence").attrib["audioLayout"] == "stereo"
    surround = ET.fromstring(build(channels=6))
    assert surround.find("./library/event/project/sequence").attrib["audioLayout"] == "surround"

    rate = ET.fromstring(build(sample_rate=44100))
    assert rate.find("./library/event/project/sequence").attrib["audioRate"] == "44.1k"


def test_empty_and_degenerate_segments_are_skipped():
    root = ET.fromstring(
        build(segments=[{"start_frame": 10, "end_frame": 10}, {"start_frame": 20, "end_frame": 25}])
    )
    clips = root.findall("./library/event/project/sequence/spine/asset-clip")
    assert len(clips) == 1
    assert clips[0].attrib["duration"] == "5/30s"
    assert clips[0].attrib["offset"] == "0s"


def test_an_audio_only_source_says_so():
    root = ET.fromstring(build(has_video=False))
    assert root.find("./resources/asset").attrib["hasVideo"] == "0"
    assert root.find("./resources/asset").attrib["hasAudio"] == "1"


def test_a_bad_version_is_refused():
    try:
        build(version="7")
    except ValueError as exc:
        assert "version" in str(exc)
    else:
        raise AssertionError("an unsupported version should be refused")


def test_a_long_edit_writes_every_clip():
    many = [{"start_frame": i * 100, "end_frame": i * 100 + 40} for i in range(1500)]
    root = ET.fromstring(build(segments=many))
    clips = root.findall("./library/event/project/sequence/spine/asset-clip")
    assert len(clips) == 1500
    assert seconds(clips[-1].attrib["offset"]) == Fraction(1499 * 40, 30)
