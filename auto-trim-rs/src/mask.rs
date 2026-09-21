//! The keep/cut mask, ported from auto-editor.
//!
//! Every function here is a line-by-line port of `src/lib/editutil.nim` in
//! auto-editor, quirks included, so the frames this service keeps are the frames
//! `auto-editor --edit audio` would keep. Anything that looks odd (the two-state
//! loop guard in `smoothing`, margins measured from *transitions* rather than run
//! bounds) is odd over there too, and matching it is the point.

/// Grow (or shrink, for negative values) every active run by `start_m` frames at
/// its head and `end_m` frames at its tail. Port of `mutMargin`.
pub fn mut_margin(arr: &mut [bool], start_m: i64, end_m: i64) {
    let len = arr.len();
    let mut start_index: Vec<usize> = Vec::new();
    let mut end_index: Vec<usize> = Vec::new();
    for j in 1..len {
        if arr[j] != arr[j - 1] {
            if arr[j] {
                start_index.push(j);
            } else {
                end_index.push(j);
            }
        }
    }

    if start_m > 0 {
        let m = start_m as usize;
        for &i in &start_index {
            for k in i.saturating_sub(m)..i {
                arr[k] = true;
            }
        }
    }
    if start_m < 0 {
        let m = start_m.unsigned_abs() as usize;
        for &i in &start_index {
            for k in i..(i + m).min(len) {
                arr[k] = false;
            }
        }
    }
    if end_m > 0 {
        let m = end_m as usize;
        for &i in &end_index {
            for k in i..(i + m).min(len) {
                arr[k] = true;
            }
        }
    }
    if end_m < 0 {
        let m = end_m.unsigned_abs() as usize;
        for &i in &end_index {
            for k in i.saturating_sub(m)..i {
                arr[k] = false;
            }
        }
    }
}

/// Drop active runs shorter than `minclip` and fill silent runs shorter than
/// `mincut`, repeatedly, until the mask stops changing. Port of `smoothing`.
pub fn smoothing(val: &mut Vec<bool>, mincut: i64, minclip: i64) {
    // A lone run shorter than both minclip and mincut flips forever (all-true
    // -> all-false -> all-true); checking two states back exits that 2-cycle.
    let mut prev: Vec<bool> = Vec::new();
    let mut prev2: Vec<bool> = Vec::new();

    while prev != *val && prev2 != *val {
        prev2 = std::mem::take(&mut prev);
        prev = val.clone();
        let mut next = prev.clone();
        let len = prev.len();

        let mut start_p = 0usize;
        let mut active = false;
        for j in 0..len {
            if prev[j] {
                if !active {
                    start_p = j;
                    active = true;
                }
                // j is the run's last index here (inclusive), not one-past like
                // the interior case below, so the run length needs the +1.
                if j == len - 1 && ((j - start_p + 1) as i64) < minclip {
                    next[start_p..len].fill(false);
                }
            } else if active {
                if ((j - start_p) as i64) < minclip {
                    next[start_p..j].fill(false);
                }
                active = false;
            }
        }

        start_p = 0;
        active = false;
        for j in 0..len {
            if !prev[j] {
                if !active {
                    start_p = j;
                    active = true;
                }
                if j == len - 1 && ((j - start_p + 1) as i64) < mincut {
                    next[start_p..len].fill(true);
                }
            } else if active {
                if ((j - start_p) as i64) < mincut {
                    next[start_p..j].fill(true);
                }
                active = false;
            }
        }

        *val = next;
    }
}

/// The active runs of the mask as `[start, end)` frame ranges — one kept clip
/// each, in order. Equivalent to auto-editor's `chunkify` + `linearClips` with
/// every silent chunk cut.
pub fn active_segments(mask: &[bool]) -> Vec<(usize, usize)> {
    let mut segments: Vec<(usize, usize)> = Vec::new();
    let mut start: Option<usize> = None;
    for (i, &keep) in mask.iter().enumerate() {
        match (keep, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                segments.push((s, i));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        segments.push((s, mask.len()));
    }
    segments
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mask(bits: &str) -> Vec<bool> {
        bits.chars().map(|c| c == '1').collect()
    }

    fn show(bits: &[bool]) -> String {
        bits.iter().map(|&b| if b { '1' } else { '0' }).collect()
    }

    #[test]
    fn margin_grows_both_edges() {
        let mut m = mask("0000011000000");
        mut_margin(&mut m, 2, 3);
        assert_eq!(show(&m), "0001111111000");
    }

    #[test]
    fn margin_clamps_at_the_bounds() {
        let mut m = mask("0110");
        mut_margin(&mut m, 5, 5);
        assert_eq!(show(&m), "1111");
    }

    #[test]
    fn a_leading_active_run_has_no_transition_to_grow_from() {
        // arr[0] == true is not a transition, so the start margin has nothing to
        // grow backwards from; only the run's tail gets a margin.
        let mut m = mask("1100000");
        mut_margin(&mut m, 2, 2);
        assert_eq!(show(&m), "1111000");
    }

    #[test]
    fn negative_margin_shrinks_the_run() {
        let mut m = mask("0011111000");
        mut_margin(&mut m, -2, -2);
        assert_eq!(show(&m), "0000100000");
    }

    #[test]
    fn smoothing_fills_a_short_silence() {
        // The 2-frame gap is shorter than mincut=3, so the two clips merge.
        let mut m = mask("11110011110000000000");
        smoothing(&mut m, 3, 2);
        assert_eq!(show(&m), "11111111110000000000");
    }

    #[test]
    fn smoothing_drops_a_short_clip() {
        // A lone 1-frame clip is shorter than minclip=2 and goes away.
        let mut m = mask("000001000000");
        smoothing(&mut m, 1, 2);
        assert_eq!(show(&m), "000000000000");
    }

    #[test]
    fn smoothing_exits_the_all_flip_cycle() {
        // Every run is shorter than both limits, so each pass flips the whole
        // mask. The two-states-back guard stops it on the input.
        let mut m = mask("0100");
        smoothing(&mut m, 10, 10);
        assert_eq!(show(&m), "0100");
    }

    #[test]
    fn a_trailing_silence_shorter_than_mincut_is_filled() {
        // The `j == len - 1` branch measures the final run inclusively, so a
        // trailing gap below mincut is kept rather than cut.
        let mut m = mask("11110");
        smoothing(&mut m, 3, 2);
        assert_eq!(show(&m), "11111");
    }

    #[test]
    fn segments_are_half_open_ranges() {
        assert_eq!(active_segments(&mask("0110011")), vec![(1, 3), (5, 7)]);
        assert_eq!(active_segments(&mask("000")), vec![]);
        assert_eq!(active_segments(&mask("111")), vec![(0, 3)]);
    }
}
