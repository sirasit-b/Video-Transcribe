"""Checks on the auto-editor timeline JSON export.

The schema is not ours: auto-editor's importer (`src/imports/json.nim`) rejects a
timebase it cannot parse, a sample rate under 100, odd or tiny dimensions, an empty
layer, a clip with a zero duration, a chunk that runs backwards. These tests hold
the export to those rules, because a file that will not import is worse than none.
"""

import json

import timeline_json

SEGMENTS = [
    {"start_frame": 0, "end_frame": 24},
    {"start_frame": 34, "end_frame": 395},
    {"start_frame": 410, "end_frame": 522},
]
TOTAL_FRAMES = 1273


def build(**overrides) -> dict:
    kwargs = dict(
        original_name="lecture 01.mp4",
        segments=SEGMENTS,
        timebase_num=30,
        timebase_den=1,
        width=1280,
        height=720,
        total_frames=TOTAL_FRAMES,
        sample_rate=48000,
        channels=2,
    )
    kwargs.update(overrides)
    return json.loads(timeline_json.build_timeline_json(**kwargs))


# ---------------------------------------------------------------------------
# v3: the full timeline
# ---------------------------------------------------------------------------


def test_v3_satisfies_what_the_importer_requires():
    document = build()
    assert document["version"] == "3"
    assert document["timebase"] == "30/1"
    assert document["samplerate"] >= 100
    assert isinstance(document["background"], str) and document["background"]
    assert len(document["resolution"]) == 2
    assert all(side >= 2 and side % 2 == 0 for side in document["resolution"])
    # Layers exist and none of them is empty.
    assert document["v"] and document["a"]
    assert all(layer for layer in document["v"] + document["a"])
    assert len(document["langs"]) == len(document["v"]) + len(document["a"])


def test_v3_clips_land_end_to_end_and_point_back_at_the_source():
    document = build()
    timeline = 0
    for clip, segment in zip(document["v"][0], SEGMENTS):
        frames = segment["end_frame"] - segment["start_frame"]
        assert clip["start"] == timeline  # where it lands
        assert clip["offset"] == segment["start_frame"]  # where it came from
        assert clip["dur"] == frames
        assert clip["stream"] == 0
        assert clip["dur"] > 0 and clip["start"] >= 0 and clip["offset"] >= 0
        timeline += frames
    assert timeline == sum(s["end_frame"] - s["start_frame"] for s in SEGMENTS)


def test_v3_audio_mirrors_the_video_layer_per_stream():
    document = build(audio_streams=2)
    assert len(document["a"]) == 2
    for stream, layer in enumerate(document["a"]):
        assert [clip["stream"] for clip in layer] == [stream] * len(SEGMENTS)
        assert [clip["start"] for clip in layer] == [clip["start"] for clip in document["v"][0]]


def test_v3_layout_follows_the_channel_count():
    assert build(channels=1)["layout"] == "mono"
    assert build(channels=2)["layout"] == "stereo"
    assert build(channels=6)["layout"] == "5.1"
    # Something unusual still has to be a layout auto-editor knows.
    assert build(channels=3)["layout"] == "stereo"


def test_v3_rounds_an_odd_resolution_down_to_even():
    # The importer refuses odd dimensions outright.
    document = build(width=1281, height=721)
    assert document["resolution"] == [1280, 720]


def test_v3_audio_only_source_has_no_video_layer():
    document = build(has_video=False)
    assert document["v"] == []
    assert len(document["a"]) == 1
    assert len(document["langs"]) == 1


def test_v3_refuses_an_edit_that_keeps_nothing():
    try:
        build(segments=[])
    except ValueError as exc:
        assert "keeps nothing" in str(exc)
    else:
        raise AssertionError("an empty timeline would not import")


def test_ntsc_timebase_is_written_as_a_rational_string():
    document = build(timebase_num=30000, timebase_den=1001)
    assert document["timebase"] == "30000/1001"
    # Frame numbers are frame numbers whatever the rate.
    assert document["v"][0][0]["dur"] == 24


# ---------------------------------------------------------------------------
# v1: the compact cut list
# ---------------------------------------------------------------------------


def test_v1_chunks_cover_the_whole_timeline_without_a_gap():
    document = build(version="1")
    assert document["version"] == "1"
    chunks = document["chunks"]

    assert chunks[0][0] == 0
    assert chunks[-1][1] == TOTAL_FRAMES
    for before, after in zip(chunks, chunks[1:]):
        assert before[1] == after[0], "chunks must be contiguous"
    for start, end, _ in chunks:
        assert end > start, "a chunk must run forwards"


def test_v1_marks_kept_and_cut_chunks_by_speed():
    chunks = build(version="1")["chunks"]
    kept = [(start, end) for start, end, speed in chunks if speed == 1.0]
    cut = [(start, end) for start, end, speed in chunks if speed == timeline_json.CUT_SPEED]

    assert kept == [(s["start_frame"], s["end_frame"]) for s in SEGMENTS]
    # The silence between and after the kept ranges: 24-34, 395-410, 522-1273.
    assert cut == [(24, 34), (395, 410), (522, TOTAL_FRAMES)]
    assert sum(end - start for start, end in kept) + sum(end - start for start, end in cut) == TOTAL_FRAMES


def test_v1_starts_with_a_cut_when_the_video_opens_on_silence():
    chunks = build(version="1", segments=[{"start_frame": 90, "end_frame": 180}])["chunks"]
    assert chunks[0] == [0, 90, timeline_json.CUT_SPEED]
    assert chunks[1] == [90, 180, 1.0]


def test_v1_needs_no_trailing_chunk_when_the_edit_runs_to_the_end():
    chunks = build(
        version="1",
        segments=[{"start_frame": 0, "end_frame": TOTAL_FRAMES}],
        )["chunks"]
    assert chunks == [[0, TOTAL_FRAMES, 1.0]]


# ---------------------------------------------------------------------------
# Both
# ---------------------------------------------------------------------------


def test_the_source_path_is_a_path_not_a_url():
    # auto-editor opens this with the filesystem, so a file:// URL would break it.
    document = build(media_path="/Volumes/Work/footage")
    assert document["templateFile"] == "/Volumes/Work/footage/lecture 01.mp4"
    assert document["v"][0][0]["src"] == "/Volumes/Work/footage/lecture 01.mp4"
    assert build(version="1", media_path="/Volumes/Work/footage")["source"].startswith("/Volumes")

    bare = build()
    assert bare["templateFile"] == "lecture 01.mp4"


def test_a_thai_name_stays_readable():
    document = build(original_name="บทเรียน 01.mp4")
    assert document["templateFile"] == "บทเรียน 01.mp4"
    assert document["auto_trim"]["name"] == "บทเรียน 01"
    # Not escaped into \\u sequences, so the file is readable as it stands.
    assert "บทเรียน" in timeline_json.build_timeline_json(
        original_name="บทเรียน 01.mp4",
        segments=SEGMENTS,
        timebase_num=30,
        timebase_den=1,
        width=1280,
        height=720,
        total_frames=TOTAL_FRAMES,
    )


def test_a_bad_version_is_refused():
    try:
        build(version="2")
    except ValueError as exc:
        assert "version" in str(exc)
    else:
        raise AssertionError("an unsupported version should be refused")


def test_both_versions_describe_the_same_kept_ranges():
    v3 = build()
    v1 = build(version="1")
    from_v3 = [(clip["offset"], clip["offset"] + clip["dur"]) for clip in v3["v"][0]]
    from_v1 = [(start, end) for start, end, speed in v1["chunks"] if speed == 1.0]
    assert from_v3 == from_v1
