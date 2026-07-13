use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum StopAlignmentFailureKind {
    #[serde(rename = "unknown_siri_stop_ids")]
    UnknownSiriStopIds,
    #[serde(rename = "consecutive_duplicate_calls")]
    ConsecutiveDuplicateCalls,
    #[serde(rename = "destination_filter_mismatch")]
    DestinationFilterMismatch,
    #[serde(rename = "all_stops_known_but_wrong_order")]
    AllStopsKnownButWrongOrder,
    #[serde(rename = "partial_alignment_90_percent")]
    PartialAlignment90Percent,
    #[serde(rename = "partial_alignment_below_90_percent")]
    PartialAlignmentBelow90Percent,
    #[serde(rename = "no_stop_alignment")]
    NoStopAlignment,
}

impl StopAlignmentFailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnknownSiriStopIds => "unknown_siri_stop_ids",
            Self::ConsecutiveDuplicateCalls => "consecutive_duplicate_calls",
            Self::DestinationFilterMismatch => "destination_filter_mismatch",
            Self::AllStopsKnownButWrongOrder => "all_stops_known_but_wrong_order",
            Self::PartialAlignment90Percent => "partial_alignment_90_percent",
            Self::PartialAlignmentBelow90Percent => "partial_alignment_below_90_percent",
            Self::NoStopAlignment => "no_stop_alignment",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct StopAlignmentDiagnostics {
    pub kind: StopAlignmentFailureKind,
    pub observed_call_count: usize,
    pub unknown_stop_ids: Vec<String>,
    pub best_exact_matched_calls: usize,
    pub best_parent_matched_calls: usize,
    pub best_parent_match_percent: u8,
    pub best_pattern_call_count: usize,
    pub first_unmatched_call_index: Option<usize>,
    pub consecutive_duplicate_count: usize,
    pub collapsed_duplicate_alignment_exists: bool,
    pub match_exists_without_destination_filter: bool,
    pub all_stops_known_in_one_pattern: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopMatchMode {
    Exact,
    ParentStation,
}

#[derive(Debug, Default)]
struct PartialAlignmentSummary {
    matched_calls: usize,
    pattern_call_count: usize,
    first_unmatched_call_index: Option<usize>,
}

pub fn diagnose_stop_alignment(
    observed_stop_ids: &[String],
    candidate_patterns: &[&[String]],
    all_patterns: &[&[String]],
    canonical_stop_ids: &HashMap<String, String>,
    destination_filter_applied: bool,
) -> StopAlignmentDiagnostics {
    let mut unknown_stop_ids = observed_stop_ids
        .iter()
        .filter(|stop_id| !canonical_stop_ids.contains_key(stop_id.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    unknown_stop_ids.sort_unstable();
    unknown_stop_ids.dedup();

    let best_exact = best_partial_alignment(
        candidate_patterns,
        observed_stop_ids,
        canonical_stop_ids,
        StopMatchMode::Exact,
    );
    let best_parent = best_partial_alignment(
        candidate_patterns,
        observed_stop_ids,
        canonical_stop_ids,
        StopMatchMode::ParentStation,
    );

    let candidate_has_full_alignment = has_full_alignment(
        candidate_patterns,
        observed_stop_ids,
        canonical_stop_ids,
    );
    let all_patterns_have_full_alignment = has_full_alignment(
        all_patterns,
        observed_stop_ids,
        canonical_stop_ids,
    );
    let match_exists_without_destination_filter = destination_filter_applied
        && !candidate_has_full_alignment
        && all_patterns_have_full_alignment;

    let consecutive_duplicate_count = observed_stop_ids
        .windows(2)
        .filter(|pair| {
            stops_match(
                &pair[0],
                &pair[1],
                canonical_stop_ids,
                StopMatchMode::ParentStation,
            )
        })
        .count();
    let collapsed_calls = collapse_consecutive_calls(observed_stop_ids, canonical_stop_ids);
    let collapsed_duplicate_alignment_exists = consecutive_duplicate_count > 0
        && has_full_alignment(
            candidate_patterns,
            &collapsed_calls,
            canonical_stop_ids,
        );

    let all_stops_known_in_one_pattern = unknown_stop_ids.is_empty()
        && candidate_patterns.iter().any(|pattern| {
            contains_all_calls_ignoring_order(
                pattern,
                observed_stop_ids,
                canonical_stop_ids,
            )
        });

    let observed_call_count = observed_stop_ids.len();
    let best_parent_match_percent = if observed_call_count == 0 {
        0
    } else {
        ((best_parent.matched_calls.saturating_mul(100) / observed_call_count).min(100)) as u8
    };

    let kind = if match_exists_without_destination_filter {
        StopAlignmentFailureKind::DestinationFilterMismatch
    } else if collapsed_duplicate_alignment_exists {
        StopAlignmentFailureKind::ConsecutiveDuplicateCalls
    } else if !unknown_stop_ids.is_empty() {
        StopAlignmentFailureKind::UnknownSiriStopIds
    } else if all_stops_known_in_one_pattern {
        StopAlignmentFailureKind::AllStopsKnownButWrongOrder
    } else if best_parent.matched_calls > 0
        && best_parent.matched_calls.saturating_mul(10)
            >= observed_call_count.saturating_mul(9)
    {
        StopAlignmentFailureKind::PartialAlignment90Percent
    } else if best_parent.matched_calls > 0 {
        StopAlignmentFailureKind::PartialAlignmentBelow90Percent
    } else {
        StopAlignmentFailureKind::NoStopAlignment
    };

    StopAlignmentDiagnostics {
        kind,
        observed_call_count,
        unknown_stop_ids,
        best_exact_matched_calls: best_exact.matched_calls,
        best_parent_matched_calls: best_parent.matched_calls,
        best_parent_match_percent,
        best_pattern_call_count: best_parent.pattern_call_count,
        first_unmatched_call_index: best_parent.first_unmatched_call_index,
        consecutive_duplicate_count,
        collapsed_duplicate_alignment_exists,
        match_exists_without_destination_filter,
        all_stops_known_in_one_pattern,
    }
}

fn best_partial_alignment(
    patterns: &[&[String]],
    observed_stop_ids: &[String],
    canonical_stop_ids: &HashMap<String, String>,
    mode: StopMatchMode,
) -> PartialAlignmentSummary {
    let mut best = PartialAlignmentSummary::default();
    let mut found_pattern = false;

    for pattern in patterns {
        let alignment = longest_common_subsequence_alignment(
            pattern,
            observed_stop_ids,
            canonical_stop_ids,
            mode,
        );
        let matched_calls = alignment.iter().filter(|index| index.is_some()).count();
        let length_difference = pattern.len().abs_diff(observed_stop_ids.len());
        let best_length_difference = best
            .pattern_call_count
            .abs_diff(observed_stop_ids.len());

        if !found_pattern
            || matched_calls > best.matched_calls
            || (matched_calls == best.matched_calls
                && length_difference < best_length_difference)
        {
            found_pattern = true;
            best = PartialAlignmentSummary {
                matched_calls,
                pattern_call_count: pattern.len(),
                first_unmatched_call_index: alignment.iter().position(Option::is_none),
            };
        }
    }

    best
}

fn longest_common_subsequence_alignment(
    pattern: &[String],
    observed_stop_ids: &[String],
    canonical_stop_ids: &HashMap<String, String>,
    mode: StopMatchMode,
) -> Vec<Option<usize>> {
    let observed_len = observed_stop_ids.len();
    let pattern_len = pattern.len();
    let width = pattern_len + 1;
    let mut lengths = vec![0_usize; (observed_len + 1) * width];

    for observed_index in 0..observed_len {
        for pattern_index in 0..pattern_len {
            let output_index = (observed_index + 1) * width + pattern_index + 1;
            lengths[output_index] = if stops_match(
                &pattern[pattern_index],
                &observed_stop_ids[observed_index],
                canonical_stop_ids,
                mode,
            ) {
                lengths[observed_index * width + pattern_index] + 1
            } else {
                lengths[observed_index * width + pattern_index + 1]
                    .max(lengths[(observed_index + 1) * width + pattern_index])
            };
        }
    }

    let mut alignment = vec![None; observed_len];
    let mut observed_index = observed_len;
    let mut pattern_index = pattern_len;

    while observed_index > 0 && pattern_index > 0 {
        let current = lengths[observed_index * width + pattern_index];
        let is_match = stops_match(
            &pattern[pattern_index - 1],
            &observed_stop_ids[observed_index - 1],
            canonical_stop_ids,
            mode,
        );

        if is_match
            && current
                == lengths[(observed_index - 1) * width + pattern_index - 1] + 1
        {
            alignment[observed_index - 1] = Some(pattern_index - 1);
            observed_index -= 1;
            pattern_index -= 1;
        } else if lengths[(observed_index - 1) * width + pattern_index]
            >= lengths[observed_index * width + pattern_index - 1]
        {
            observed_index -= 1;
        } else {
            pattern_index -= 1;
        }
    }

    alignment
}

fn has_full_alignment(
    patterns: &[&[String]],
    observed_stop_ids: &[String],
    canonical_stop_ids: &HashMap<String, String>,
) -> bool {
    patterns.iter().any(|pattern| {
        has_ordered_alignment(
            pattern,
            observed_stop_ids,
            canonical_stop_ids,
            StopMatchMode::Exact,
        ) || has_ordered_alignment(
            pattern,
            observed_stop_ids,
            canonical_stop_ids,
            StopMatchMode::ParentStation,
        )
    })
}

fn has_ordered_alignment(
    pattern: &[String],
    observed_stop_ids: &[String],
    canonical_stop_ids: &HashMap<String, String>,
    mode: StopMatchMode,
) -> bool {
    let mut search_from = 0_usize;

    for observed_stop_id in observed_stop_ids {
        let Some(offset) = pattern[search_from..].iter().position(|gtfs_stop_id| {
            stops_match(gtfs_stop_id, observed_stop_id, canonical_stop_ids, mode)
        }) else {
            return false;
        };

        search_from += offset + 1;
    }

    true
}

fn collapse_consecutive_calls(
    observed_stop_ids: &[String],
    canonical_stop_ids: &HashMap<String, String>,
) -> Vec<String> {
    let mut collapsed: Vec<String> = Vec::with_capacity(observed_stop_ids.len());

    for stop_id in observed_stop_ids {
        if collapsed.last().is_some_and(|previous| {
            stops_match(
                previous,
                stop_id,
                canonical_stop_ids,
                StopMatchMode::ParentStation,
            )
        }) {
            continue;
        }
        collapsed.push(stop_id.clone());
    }

    collapsed
}

fn contains_all_calls_ignoring_order(
    pattern: &[String],
    observed_stop_ids: &[String],
    canonical_stop_ids: &HashMap<String, String>,
) -> bool {
    let mut available = HashMap::<String, usize>::new();
    for stop_id in pattern {
        *available
            .entry(canonical_stop_id_owned(stop_id, canonical_stop_ids))
            .or_default() += 1;
    }

    for stop_id in observed_stop_ids {
        let canonical = canonical_stop_id_owned(stop_id, canonical_stop_ids);
        let Some(count) = available.get_mut(&canonical) else {
            return false;
        };
        if *count == 0 {
            return false;
        }
        *count -= 1;
    }

    true
}

fn stops_match(
    left: &str,
    right: &str,
    canonical_stop_ids: &HashMap<String, String>,
    mode: StopMatchMode,
) -> bool {
    match mode {
        StopMatchMode::Exact => left == right,
        StopMatchMode::ParentStation => {
            canonical_stop_ids
                .get(left)
                .map(String::as_str)
                .unwrap_or(left)
                == canonical_stop_ids
                    .get(right)
                    .map(String::as_str)
                    .unwrap_or(right)
        }
    }
}

fn canonical_stop_id_owned(
    stop_id: &str,
    canonical_stop_ids: &HashMap<String, String>,
) -> String {
    canonical_stop_ids
        .get(stop_id)
        .cloned()
        .unwrap_or_else(|| stop_id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn self_canonical(ids: &[&str]) -> HashMap<String, String> {
        ids.iter()
            .map(|id| ((*id).to_string(), (*id).to_string()))
            .collect()
    }

    fn owned(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| (*id).to_string()).collect()
    }

    #[test]
    fn classifies_unknown_siri_stop_ids() {
        let observed = owned(&["A", "unknown", "C"]);
        let pattern = owned(&["A", "B", "C"]);
        let canonical = self_canonical(&["A", "B", "C"]);
        let diagnostics = diagnose_stop_alignment(
            &observed,
            &[pattern.as_slice()],
            &[pattern.as_slice()],
            &canonical,
            false,
        );

        assert_eq!(diagnostics.kind, StopAlignmentFailureKind::UnknownSiriStopIds);
        assert_eq!(diagnostics.unknown_stop_ids, vec!["unknown"]);
        assert_eq!(diagnostics.best_parent_matched_calls, 2);
    }

    #[test]
    fn classifies_consecutive_duplicate_calls_when_collapsing_fixes_alignment() {
        let observed = owned(&["A", "B", "B", "C"]);
        let pattern = owned(&["A", "B", "C"]);
        let canonical = self_canonical(&["A", "B", "C"]);
        let diagnostics = diagnose_stop_alignment(
            &observed,
            &[pattern.as_slice()],
            &[pattern.as_slice()],
            &canonical,
            false,
        );

        assert_eq!(
            diagnostics.kind,
            StopAlignmentFailureKind::ConsecutiveDuplicateCalls
        );
        assert_eq!(diagnostics.consecutive_duplicate_count, 1);
        assert!(diagnostics.collapsed_duplicate_alignment_exists);
    }

    #[test]
    fn classifies_destination_filter_mismatch() {
        let observed = owned(&["A", "B", "C"]);
        let filtered_pattern = owned(&["A", "X", "C"]);
        let matching_pattern = owned(&["A", "B", "C"]);
        let canonical = self_canonical(&["A", "B", "C", "X"]);
        let diagnostics = diagnose_stop_alignment(
            &observed,
            &[filtered_pattern.as_slice()],
            &[filtered_pattern.as_slice(), matching_pattern.as_slice()],
            &canonical,
            true,
        );

        assert_eq!(
            diagnostics.kind,
            StopAlignmentFailureKind::DestinationFilterMismatch
        );
        assert!(diagnostics.match_exists_without_destination_filter);
    }

    #[test]
    fn classifies_known_stops_in_the_wrong_order() {
        let observed = owned(&["A", "C", "B"]);
        let pattern = owned(&["A", "B", "C"]);
        let canonical = self_canonical(&["A", "B", "C"]);
        let diagnostics = diagnose_stop_alignment(
            &observed,
            &[pattern.as_slice()],
            &[pattern.as_slice()],
            &canonical,
            false,
        );

        assert_eq!(
            diagnostics.kind,
            StopAlignmentFailureKind::AllStopsKnownButWrongOrder
        );
        assert!(diagnostics.all_stops_known_in_one_pattern);
    }

    #[test]
    fn classifies_ninety_percent_partial_alignment() {
        let observed = owned(&["A", "B", "C", "D", "E", "F", "G", "H", "I", "J"]);
        let pattern = owned(&["A", "B", "C", "D", "E", "F", "G", "H", "I"]);
        let canonical = self_canonical(&["A", "B", "C", "D", "E", "F", "G", "H", "I", "J"]);
        let diagnostics = diagnose_stop_alignment(
            &observed,
            &[pattern.as_slice()],
            &[pattern.as_slice()],
            &canonical,
            false,
        );

        assert_eq!(
            diagnostics.kind,
            StopAlignmentFailureKind::PartialAlignment90Percent
        );
        assert_eq!(diagnostics.best_parent_match_percent, 90);
        assert_eq!(diagnostics.first_unmatched_call_index, Some(9));
    }
}
