from caption_timeline import shift_captions, to_cut_time, _ranges


def kept(*pairs):
    return [{"start": start, "end": end} for start, end in pairs]


def cue(start, end, text="พูดอะไรสักอย่าง"):
    return {"start": start, "end": end, "text": text}


def test_a_time_before_any_cut_is_where_it_was():
    ranges = _ranges(kept((0, 10), (20, 30)))
    assert to_cut_time(5.0, ranges, forward=True) == 5.0


def test_a_time_after_a_cut_moves_up_by_what_was_removed():
    # Ten seconds were taken out between 10s and 20s, so 25s is 15s in the cut.
    ranges = _ranges(kept((0, 10), (20, 30)))
    assert to_cut_time(25.0, ranges, forward=True) == 15.0


def test_several_cuts_add_up():
    ranges = _ranges(kept((0, 5), (10, 15), (30, 40)))
    # 5 + 5 kept before 35s, plus 5 into the last range.
    assert to_cut_time(35.0, ranges, forward=True) == 15.0


def test_a_cue_that_straddles_a_cut_does_not_grow_to_cover_it():
    # Speech from 8s to 22s across a cut of 10s-20s: in the cut it runs from 8s
    # to 12s, the two seconds either side joined together — not fourteen seconds.
    captions = [cue(8.0, 22.0)]
    shifted = shift_captions(captions, kept((0, 10), (20, 30)))
    assert shifted[0]["start"] == 8.0
    assert shifted[0]["end"] == 12.0


def test_a_cue_wholly_inside_a_cut_is_dropped():
    # There is nowhere for it to go, and what was cut was silence anyway.
    assert shift_captions([cue(12.0, 18.0)], kept((0, 10), (20, 30))) == []


def test_a_cue_keeps_its_text_and_anything_else_on_it():
    shifted = shift_captions(
        [{"start": 21.0, "end": 22.0, "text": "สวัสดี", "speaker": "A"}],
        kept((0, 10), (20, 30)),
    )
    assert shifted == [{"start": 11.0, "end": 12.0, "text": "สวัสดี", "speaker": "A"}]


def test_cues_after_the_last_kept_range_are_dropped():
    assert shift_captions([cue(50.0, 52.0)], kept((0, 10), (20, 30))) == []


def test_an_untrimmed_recording_keeps_its_own_timings():
    captions = [cue(1.0, 2.0), cue(3.0, 4.0)]
    assert shift_captions(captions, []) == captions
    assert shift_captions(captions, None) == captions


def test_the_whole_transcript_stays_in_order_and_inside_the_cut():
    # A real shape: a ten-minute take cut into five pieces, cues throughout.
    ranges = kept((0, 60), (90, 150), (200, 260), (300, 360), (400, 460))
    captions = [cue(start, start + 2.0) for start in range(0, 460, 7)]
    shifted = shift_captions(captions, ranges)

    total_kept = 5 * 60.0
    assert shifted, "some of it has to survive"
    assert all(0 <= c["start"] < c["end"] <= total_kept for c in shifted)
    starts = [c["start"] for c in shifted]
    assert starts == sorted(starts), "the cut does not reorder what was said"
    # Every cue overlapping something that was kept is still there; a cue that
    # begins in a silence and runs on into speech counts, because the speech does.
    overlapping = [
        c
        for c in captions
        if any(c["start"] < r["end"] and c["end"] > r["start"] for r in ranges)
    ]
    assert len(shifted) == len(overlapping)


def test_ranges_out_of_order_or_empty_are_tidied():
    ranges = _ranges(
        [
            {"start": 20, "end": 30},
            {"start": 0, "end": 10},
            {"start": 5, "end": 5},
            {"start": "x", "end": 1},
            {},
        ]
    )
    assert ranges == [(0.0, 10.0), (20.0, 30.0)]
