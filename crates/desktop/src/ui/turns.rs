//! Strict turn navigation over visible desktop timeline rows.
use crate::timeline::TimelineRow;

pub fn turn_rows(rows: &[TimelineRow]) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter_map(|(index, row)| row.turn_start.then_some(index))
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnDirection {
    Previous,
    Next,
}

pub fn destination(
    turn_rows: &[usize],
    top_row: usize,
    within_row: bool,
    direction: TurnDirection,
) -> Option<usize> {
    match direction {
        TurnDirection::Previous => turn_rows
            .iter()
            .copied()
            .filter(|row| *row < top_row || (within_row && *row == top_row))
            .max(),
        TurnDirection::Next => turn_rows.iter().copied().filter(|row| *row > top_row).min(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn destinations_are_strict_row_granular_and_never_wrap() {
        use TurnDirection::*;
        let rows = [0, 5, 10];
        for (row, within, previous, next) in [
            (0, false, None, Some(5)),
            (0, true, Some(0), Some(5)),
            (3, false, Some(0), Some(5)),
            (5, false, Some(0), Some(10)),
            (5, true, Some(5), Some(10)),
            (9, true, Some(5), Some(10)),
            (10, false, Some(5), None),
            (99, false, Some(10), None),
        ] {
            assert_eq!(destination(&rows, row, within, Previous), previous);
            assert_eq!(destination(&rows, row, within, Next), next);
        }
        assert_eq!(destination(&[], 0, false, Previous), None);
        assert_eq!(destination(&[], 0, true, Next), None);
    }
}
