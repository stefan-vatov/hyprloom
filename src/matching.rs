//! Window assignment strategies used by reconciliation.
//!
//! This module deliberately knows nothing about Hyprland or application
//! identity.  The restore layer supplies a matrix whose positive entries are
//! eligible target/window matches; this module only decides which eligible
//! edges to keep.

/// Strategy used to turn scored candidate matches into a one-to-one plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MatchingStrategy {
    /// Maximize the total score across the whole target set.
    #[default]
    Global,
    /// Take the highest-scoring remaining edge at each step.
    ///
    /// This is useful when a caller wants simple, predictable first-choice
    /// behavior.  Ties are resolved by target index and then current-window
    /// index, so repeated runs over the same observation are deterministic.
    Greedy,
}

/// Assign candidates according to `strategy`.
///
/// Each row represents one saved target and each column one currently open
/// window.  A positive score is an eligible match; zero means that the edge is
/// not eligible.  The returned vector has one entry per target and contains at
/// most one assignment for each current window.
pub(crate) fn assign(scores: &[Vec<i32>], strategy: MatchingStrategy) -> Vec<Option<usize>> {
    let current_count = scores.first().map_or(0, Vec::len);
    debug_assert!(scores.iter().all(|row| row.len() == current_count));

    match strategy {
        MatchingStrategy::Global => maximum_weight_assignment(scores),
        MatchingStrategy::Greedy => greedy_assignment(scores),
    }
}

/// Solve a rectangular maximum-weight assignment problem with optional
/// unmatched targets/windows represented by zero-weight dummy edges.
// The Hungarian algorithm's nested relaxation loops mirror the standard
// primal-dual formulation and are easier to audit when kept together.
#[allow(clippy::excessive_nesting)]
fn maximum_weight_assignment(scores: &[Vec<i32>]) -> Vec<Option<usize>> {
    let target_count = scores.len();
    if target_count == 0 {
        return vec![];
    }
    let current_count = scores.first().map_or(0, Vec::len);
    let size = target_count.max(current_count);
    let mut weights = vec![vec![0_i64; size]; size];
    for (target_index, row) in scores.iter().enumerate() {
        for (current_index, score) in row.iter().enumerate() {
            weights[target_index][current_index] = i64::from(*score);
        }
    }

    // This is the standard primal-dual Hungarian formulation for minimising
    // costs. Negating weights turns it into the maximum-weight variant.
    let infinity = i64::MAX / 4;
    let mut u = vec![0_i64; size + 1];
    let mut v = vec![0_i64; size + 1];
    let mut p = vec![0_usize; size + 1];
    let mut way = vec![0_usize; size + 1];

    for row in 1..=size {
        p[0] = row;
        let mut column = 0;
        let mut minv = vec![infinity; size + 1];
        let mut used = vec![false; size + 1];

        loop {
            used[column] = true;
            let current_row = p[column];
            let mut delta = infinity;
            let mut next_column = 0;

            for candidate_column in 1..=size {
                if used[candidate_column] {
                    continue;
                }
                let cost = -weights[current_row - 1][candidate_column - 1] - u[current_row] - v[candidate_column];
                if cost < minv[candidate_column] {
                    minv[candidate_column] = cost;
                    way[candidate_column] = column;
                }
                if minv[candidate_column] < delta {
                    delta = minv[candidate_column];
                    next_column = candidate_column;
                }
            }

            for candidate_column in 0..=size {
                if used[candidate_column] {
                    u[p[candidate_column]] += delta;
                    v[candidate_column] -= delta;
                } else if candidate_column > 0 {
                    minv[candidate_column] -= delta;
                }
            }

            column = next_column;
            if p[column] == 0 {
                break;
            }
        }

        loop {
            let previous_column = way[column];
            p[column] = p[previous_column];
            column = previous_column;
            if column == 0 {
                break;
            }
        }
    }

    let mut assignment = vec![None; target_count];
    for (column, assigned_row) in p.iter().enumerate().skip(1).take(size) {
        if *assigned_row != 0 {
            let target_index = *assigned_row - 1;
            let current_index = column - 1;
            if target_index < target_count && current_index < current_count && scores[target_index][current_index] > 0 {
                assignment[target_index] = Some(current_index);
            }
        }
    }
    assignment
}

/// Select the highest-scoring unclaimed edge until no eligible edge remains.
// Greedy matching walks a sorted edge list while updating both used sets; the
// nested loop is the algorithm rather than incidental control flow.
#[allow(clippy::excessive_nesting)]
fn greedy_assignment(scores: &[Vec<i32>]) -> Vec<Option<usize>> {
    let target_count = scores.len();
    let current_count = scores.first().map_or(0, Vec::len);
    let mut edges = Vec::new();

    for (target_index, row) in scores.iter().enumerate() {
        for (current_index, score) in row.iter().enumerate() {
            if *score > 0 {
                edges.push((*score, target_index, current_index));
            }
        }
    }

    // Descending score gives greedy its intended behavior. Ascending indexes
    // make equal-score decisions stable and easy to explain in diagnostics.
    edges.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)).then(left.2.cmp(&right.2)));

    let mut assignment = vec![None; target_count];
    let mut used_targets = vec![false; target_count];
    let mut used_current = vec![false; current_count];
    for (_, target_index, current_index) in edges {
        if !used_targets[target_index] && !used_current[current_index] {
            used_targets[target_index] = true;
            used_current[current_index] = true;
            assignment[target_index] = Some(current_index);
        }
    }
    assignment
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_assignment_can_trade_a_first_choice_for_more_matches() {
        let scores = vec![vec![100, 99], vec![98, 0]];
        assert_eq!(assign(&scores, MatchingStrategy::Global), vec![Some(1), Some(0)]);
    }

    #[test]
    fn greedy_assignment_takes_the_highest_remaining_edge() {
        let scores = vec![vec![100, 99], vec![98, 0]];
        assert_eq!(assign(&scores, MatchingStrategy::Greedy), vec![Some(0), None]);
    }

    #[test]
    fn greedy_assignment_is_deterministic_for_equal_scores() {
        let scores = vec![vec![100, 100], vec![100, 100]];
        assert_eq!(assign(&scores, MatchingStrategy::Greedy), vec![Some(0), Some(1)]);
    }

    #[test]
    fn assignment_ignores_zero_edges() {
        let scores = vec![vec![0, 0], vec![0, 10]];
        assert_eq!(assign(&scores, MatchingStrategy::Global), vec![None, Some(1)]);
        assert_eq!(assign(&scores, MatchingStrategy::Greedy), vec![None, Some(1)]);
    }
}
