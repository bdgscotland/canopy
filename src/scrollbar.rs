//! Scrollbar thumb arithmetic, shared by every scrollbar in the app: the
//! tree's vertical and horizontal bars and the terminal's scrollback bar.
//!
//! One copy on purpose. The renderer and the mouse handler must agree on
//! where the thumb is, and independent copies of this arithmetic drift --
//! the tree's vertical bar had exactly that bug before the math was
//! centralized on FileTree, and this module is the same idea one level up.

/// Where the thumb sits, as (position, length) in track cells, or None when
/// everything fits and no scrollbar should be drawn.
pub fn thumb(total: usize, visible: usize, offset: usize) -> Option<(usize, usize)> {
    if visible == 0 || total <= visible {
        return None;
    }
    let len = ((visible * visible) / total).max(1);
    let max_offset = total - visible;
    let travel = visible - len;
    let pos = if max_offset == 0 {
        0
    } else {
        (offset.min(max_offset) * travel) / max_offset
    };
    Some((pos.min(travel), len))
}

/// The content offset that puts the thumb's leading edge on `pos` -- the
/// inverse of [`thumb`], for drags. Returns 0 when no scrollbar exists.
pub fn offset_for_thumb_pos(pos: usize, total: usize, visible: usize) -> usize {
    let Some((_, len)) = thumb(total, visible, 0) else {
        return 0;
    };
    let max_offset = total - visible;
    let travel = visible - len;
    if travel == 0 {
        0
    } else {
        (pos.min(travel) * max_offset) / travel
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_thumb_when_everything_fits() {
        assert!(thumb(10, 10, 0).is_none(), "exact fit needs no bar");
        assert!(thumb(5, 10, 0).is_none(), "less than a screenful");
        assert!(thumb(10, 0, 0).is_none(), "zero-size track");
    }

    #[test]
    fn the_thumb_stays_inside_the_track_at_every_offset() {
        let (total, visible) = (100, 12);
        for offset in 0..=(total - visible) {
            let (pos, len) = thumb(total, visible, offset).expect("thumb");
            assert!(len >= 1, "thumb must be grabbable");
            assert!(pos + len <= visible, "escaped the track at offset {offset}");
        }
        assert_eq!(thumb(total, visible, 0).unwrap().0, 0, "top at offset 0");
        let (pos, len) = thumb(total, visible, total - visible).unwrap();
        assert_eq!(pos + len, visible, "full scroll parks the thumb at the end");
    }

    #[test]
    fn an_offset_past_the_end_is_clamped_not_wrapped() {
        let (pos, len) = thumb(100, 12, 10_000).expect("thumb");
        assert_eq!(pos + len, 12, "overshoot clamps to the end of the track");
    }

    /// Endpoints must be exact; interior positions may quantize by one cell
    /// (integer division), exactly as the tree's vertical drag always has.
    #[test]
    fn offset_for_thumb_pos_inverts_thumb() {
        let (total, visible) = (97, 12);
        let (_, len) = thumb(total, visible, 0).unwrap();
        let travel = visible - len;
        assert_eq!(offset_for_thumb_pos(0, total, visible), 0);
        assert_eq!(
            offset_for_thumb_pos(travel, total, visible),
            total - visible,
            "dragging to the end reaches the last line"
        );
        for pos in 0..=travel {
            let offset = offset_for_thumb_pos(pos, total, visible);
            let (round_trip, _) = thumb(total, visible, offset).unwrap();
            assert!(
                round_trip <= pos && pos - round_trip <= 1,
                "drag to {pos} drew the thumb at {round_trip}"
            );
        }
    }

    #[test]
    fn no_scrollbar_means_offset_zero() {
        assert_eq!(offset_for_thumb_pos(3, 5, 10), 0);
        assert_eq!(offset_for_thumb_pos(3, 10, 0), 0);
    }
}
