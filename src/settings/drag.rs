//! Where a dragged binding row lands.

/// New index of row `from` dropped with the pointer at `drop_y` (list coordinates).
pub fn reorder(len: usize, from: usize, drop_y: f32, row_h: f32) -> usize {
    if len == 0 || row_h <= 0.0 {
        return from;
    }
    // The gap the pointer is nearest to: a row's lower half counts as below it.
    let gap = (drop_y / row_h + 0.5).floor().clamp(0.0, len as f32) as usize;
    if gap > from { gap - 1 } else { gap }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: f32 = 40.0;

    #[test]
    fn drop_above_first() {
        assert_eq!(reorder(3, 2, -10.0, H), 0);
        assert_eq!(reorder(3, 2, 5.0, H), 0);
    }

    #[test]
    fn drop_between() {
        // The gap between rows 1 and 2 sits at y = 80.
        assert_eq!(reorder(4, 0, 80.0, H), 1);
        assert_eq!(reorder(4, 3, 40.0, H), 1);
    }

    #[test]
    fn drop_past_end() {
        assert_eq!(reorder(3, 0, 500.0, H), 2);
    }

    #[test]
    fn half_row_boundary() {
        // Exactly at row 1's midpoint counts as below it.
        assert_eq!(reorder(3, 0, 60.0, H), 1);
        assert_eq!(reorder(3, 0, 59.0, H), 0);
        assert_eq!(reorder(3, 2, 20.0, H), 1);
    }

    #[test]
    fn same_slot_no_move() {
        assert_eq!(reorder(3, 1, 60.0, H), 1);
        assert_eq!(reorder(3, 1, 45.0, H), 1);
        assert_eq!(reorder(3, 1, 75.0, H), 1);
    }
}
