use crate::domain::document::{TranslatableUnit, UnitContext, UnitKind};
use strsim::normalized_levenshtein;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreviousUnit {
    pub stable_id: String,
    pub kind: UnitKind,
    pub context: UnitContext,
    pub ordinal: usize,
    pub source: String,
    pub translation: String,
    pub trusted: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MatchKind {
    Exact,
    Moved,
    Fuzzy(f64),
    New,
    Ambiguous,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UnitMatch {
    pub current_id: String,
    pub stable_id: Option<String>,
    pub previous_source: Option<String>,
    pub previous_translation: Option<String>,
    pub trusted_reuse: bool,
    pub kind: MatchKind,
}

fn normalized(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn match_units(previous: &[PreviousUnit], current: &[TranslatableUnit]) -> Vec<UnitMatch> {
    match_units_with_stable_ids(previous, current, &[])
}

pub fn match_units_with_stable_ids(
    previous: &[PreviousUnit],
    current: &[TranslatableUnit],
    stable_ids: &[String],
) -> Vec<UnitMatch> {
    let mut used = vec![false; previous.len()];
    let mut matches = Vec::with_capacity(current.len());

    for (ordinal, unit) in current.iter().enumerate() {
        if let Some(stable_id) = stable_ids.get(ordinal) {
            if let Some((index, candidate)) =
                previous.iter().enumerate().find(|(index, candidate)| {
                    !used[*index]
                        && candidate.stable_id == *stable_id
                        && candidate.kind == unit.kind
                        && candidate.context == unit.context
                        && normalized(&candidate.source) == normalized(&unit.source)
                })
            {
                used[index] = true;
                matches.push(UnitMatch {
                    current_id: unit.id.clone(),
                    stable_id: Some(candidate.stable_id.clone()),
                    previous_source: Some(candidate.source.clone()),
                    previous_translation: Some(candidate.translation.clone()),
                    trusted_reuse: candidate.trusted && candidate.source == unit.source,
                    kind: MatchKind::Exact,
                });
                continue;
            }
        }
        let exact: Vec<_> = previous
            .iter()
            .enumerate()
            .filter(|(index, candidate)| {
                !used[*index]
                    && candidate.kind == unit.kind
                    && candidate.context == unit.context
                    && normalized(&candidate.source) == normalized(&unit.source)
            })
            .collect();
        if exact.len() == 1 {
            let (index, candidate) = exact[0];
            used[index] = true;
            matches.push(UnitMatch {
                current_id: unit.id.clone(),
                stable_id: Some(candidate.stable_id.clone()),
                previous_source: Some(candidate.source.clone()),
                previous_translation: Some(candidate.translation.clone()),
                trusted_reuse: candidate.trusted && candidate.source == unit.source,
                kind: if candidate.ordinal == ordinal {
                    MatchKind::Exact
                } else {
                    MatchKind::Moved
                },
            });
            continue;
        }
        if exact.len() > 1 {
            matches.push(UnitMatch {
                current_id: unit.id.clone(),
                stable_id: None,
                previous_source: None,
                previous_translation: None,
                trusted_reuse: false,
                kind: MatchKind::Ambiguous,
            });
            continue;
        }

        let source = normalized(&unit.source);
        let mut candidates: Vec<_> = previous
            .iter()
            .enumerate()
            .filter(|(index, candidate)| {
                !used[*index] && candidate.kind == unit.kind && candidate.context == unit.context
            })
            .map(|(index, candidate)| {
                let text_score = normalized_levenshtein(&normalized(&candidate.source), &source);
                let distance = candidate.ordinal.abs_diff(ordinal) as f64;
                let position_score = 1.0 / (1.0 + distance);
                (index, candidate, text_score * 0.85 + position_score * 0.15)
            })
            .filter(|(_, _, score)| *score >= 0.72)
            .collect();
        candidates.sort_by(|left, right| {
            right
                .2
                .total_cmp(&left.2)
                .then_with(|| left.1.stable_id.cmp(&right.1.stable_id))
        });
        if let Some((index, candidate, score)) = candidates.first().copied() {
            let ambiguous = candidates
                .get(1)
                .is_some_and(|next| (score - next.2).abs() < 0.03);
            if ambiguous {
                matches.push(UnitMatch {
                    current_id: unit.id.clone(),
                    stable_id: None,
                    previous_source: None,
                    previous_translation: None,
                    trusted_reuse: false,
                    kind: MatchKind::Ambiguous,
                });
            } else {
                used[index] = true;
                matches.push(UnitMatch {
                    current_id: unit.id.clone(),
                    stable_id: Some(candidate.stable_id.clone()),
                    previous_source: Some(candidate.source.clone()),
                    previous_translation: Some(candidate.translation.clone()),
                    trusted_reuse: false,
                    kind: MatchKind::Fuzzy(score),
                });
            }
            continue;
        }
        matches.push(UnitMatch {
            current_id: unit.id.clone(),
            stable_id: None,
            previous_source: None,
            previous_translation: None,
            trusted_reuse: false,
            kind: MatchKind::New,
        });
    }
    matches
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::markdown::extract_units;

    fn previous(id: &str, ordinal: usize, source: &str) -> PreviousUnit {
        PreviousUnit {
            stable_id: id.into(),
            kind: UnitKind::Paragraph,
            context: UnitContext::markdown(),
            ordinal,
            source: source.into(),
            translation: format!("translated {id}"),
            trusted: true,
        }
    }

    #[test]
    fn exact_move_reuses_but_fuzzy_needs_review() {
        let current = extract_units("First paragraph!\n\nSame text.\n");
        let result = match_units(
            &[
                previous("same", 0, "Same text."),
                previous("changed", 1, "First paragraph.\n"),
            ],
            &current,
        );
        assert!(matches!(result[0].kind, MatchKind::Fuzzy(_)));
        assert!(!result[0].trusted_reuse);
        assert_eq!(result[1].kind, MatchKind::Moved);
        assert!(result[1].trusted_reuse);
    }

    #[test]
    fn context_changes_do_not_match_and_whitespace_changes_do_not_reuse() {
        let current = extract_units("Same text.\n");
        let mut old = previous("same", 0, "Same text.");
        old.context.version = "unknown".into();
        assert_eq!(match_units(&[old], &current)[0].kind, MatchKind::New);
        let result = match_units(&[previous("same", 0, "Same  text.")], &current);
        assert_eq!(result[0].kind, MatchKind::Exact);
        assert_eq!(result[0].stable_id.as_deref(), Some("same"));
        assert!(!result[0].trusted_reuse);
    }

    #[test]
    fn duplicate_exact_text_is_ambiguous() {
        let current = extract_units("Repeated.\n");
        let result = match_units(
            &[
                previous("a", 0, "Repeated.\n"),
                previous("b", 1, "Repeated.\n"),
            ],
            &current,
        );
        assert_eq!(result[0].kind, MatchKind::Ambiguous);
    }

    #[test]
    fn stable_identity_disambiguates_unchanged_duplicate_text() {
        let current = extract_units("Repeated.\n\nRepeated.\n");
        let result = match_units_with_stable_ids(
            &[
                previous("stable-a", 0, "Repeated.\n"),
                previous("stable-b", 1, "Repeated.\n"),
            ],
            &current,
            &["stable-a".into(), "stable-b".into()],
        );
        assert_eq!(result[0].stable_id.as_deref(), Some("stable-a"));
        assert_eq!(result[1].stable_id.as_deref(), Some("stable-b"));
        assert_eq!(result[0].kind, MatchKind::Exact);
        assert_eq!(result[1].kind, MatchKind::Exact);
    }
}
