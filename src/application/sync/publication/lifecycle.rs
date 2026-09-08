use crate::application::contracts::PublicationPayload;
use crate::application::ports::PublicationPromotionInput;
use crate::domain::model::PublicationState;
use anyhow::{Result, bail};

pub(super) enum Preparation<'a> {
    Unprepared,
    Prepared {
        commit: &'a str,
        expected_remote_tip: Option<&'a str>,
    },
}

pub(super) fn preparation(payload: &PublicationPayload) -> Result<Preparation<'_>> {
    match payload.commit.as_deref() {
        Some("") => bail!("publication candidate commit must not be empty"),
        Some(commit) => Ok(Preparation::Prepared {
            commit,
            // Old durable payloads may not contain the remote-tip field.
            expected_remote_tip: payload
                .expected_remote_tip
                .as_ref()
                .and_then(|tip| tip.as_deref()),
        }),
        None if payload
            .expected_remote_tip
            .as_ref()
            .is_some_and(Option::is_some) =>
        {
            bail!("unprepared publication contains a remote-tip receipt")
        }
        None => Ok(Preparation::Unprepared),
    }
}

/// Verified merge evidence is collected before any database state is changed.
pub(super) enum MergeAssessment {
    Verified(Vec<i64>),
    Superseded,
    Unchanged,
}

impl MergeAssessment {
    pub(super) fn state(&self) -> Option<PublicationState> {
        match self {
            Self::Superseded => Some(PublicationState::Superseded),
            Self::Verified(_) | Self::Unchanged => None,
        }
    }

    pub(super) fn promotion<'a>(
        &'a self,
        candidate_commit: &'a str,
    ) -> Option<PublicationPromotionInput<'a>> {
        match self {
            Self::Verified(verified_zero_unit_contents) => Some(PublicationPromotionInput {
                candidate_commit,
                provenance: "github_merged",
                verified_zero_unit_contents,
            }),
            Self::Superseded | Self::Unchanged => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PullRequestLifecycle {
    Merged,
    Draft,
    Open,
    Closed,
}
impl PullRequestLifecycle {
    pub(super) fn observed(state: &str, draft: bool) -> Self {
        if state.eq_ignore_ascii_case("merged") {
            Self::Merged
        } else if state.eq_ignore_ascii_case("open") && draft {
            Self::Draft
        } else if state.eq_ignore_ascii_case("open") {
            Self::Open
        } else {
            Self::Closed
        }
    }
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Merged => "merged",
            Self::Draft => "draft",
            Self::Open => "open",
            Self::Closed => "closed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Preparation, preparation};
    use crate::application::sync::publication::codec;

    const LEGACY: &str = r#"{"files":[],"language":"fr","policy_fingerprint":"policy","run_id":"run","source_revision":"rev"}"#;

    #[test]
    fn legacy_unprepared_receipt_keeps_identical_effect_key_bytes() {
        let payload = codec::decode(LEGACY).unwrap();
        assert!(matches!(
            preparation(&payload).unwrap(),
            Preparation::Unprepared
        ));
        assert_eq!(codec::encode(&payload).unwrap(), LEGACY);
    }

    #[test]
    fn prepared_recovery_never_becomes_a_new_prepare_operation() {
        let mut payload = codec::decode(LEGACY).unwrap();
        payload.commit = Some("persisted-commit".into());
        // Older receipts legitimately lack the remote-tip field.
        assert!(matches!(
            preparation(&payload).unwrap(),
            Preparation::Prepared {
                commit: "persisted-commit",
                expected_remote_tip: None
            }
        ));
        payload.expected_remote_tip = Some(Some("remote-before-push".into()));
        let recovered = codec::decode(&codec::encode(&payload).unwrap()).unwrap();
        assert!(matches!(
            preparation(&recovered).unwrap(),
            Preparation::Prepared {
                commit: "persisted-commit",
                expected_remote_tip: Some("remote-before-push")
            }
        ));
        let rejected = codec::rejected(&recovered);
        assert!(
            codec::decode(&codec::encode(&rejected).unwrap())
                .unwrap()
                .superseded_reason
                .is_some()
        );
    }

    #[test]
    fn explicit_absent_remote_branch_survives_receipt_round_trip() {
        let receipt = r#"{"commit":"persisted-commit","expected_remote_tip":null,"files":[],"language":"fr","policy_fingerprint":"policy","run_id":"run","source_revision":"rev"}"#;
        let payload = codec::decode(receipt).unwrap();
        assert_eq!(payload.expected_remote_tip, Some(None));
        assert_eq!(codec::encode(&payload).unwrap(), receipt);
    }

    #[test]
    fn legacy_null_candidate_and_remote_tip_remain_unprepared() {
        let receipt = r#"{"commit":null,"expected_remote_tip":null,"files":[],"language":"fr","policy_fingerprint":"policy","run_id":"run","source_revision":"rev"}"#;
        let payload = codec::decode(receipt).unwrap();
        assert!(matches!(
            preparation(&payload).unwrap(),
            Preparation::Unprepared
        ));
    }

    #[test]
    fn inconsistent_recovery_receipts_are_rejected() {
        let mut payload = codec::decode(LEGACY).unwrap();
        payload.expected_remote_tip = Some(Some("tip".into()));
        assert!(preparation(&payload).is_err());
        payload.commit = Some(String::new());
        assert!(preparation(&payload).is_err());
    }
}
