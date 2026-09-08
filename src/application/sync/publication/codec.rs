//! Compatibility codec for the existing SQLite publication payload schema.
use crate::application::contracts::{DocumentIdentity, PublicationPayload};
use anyhow::Result;

pub(super) fn encode(payload: &PublicationPayload) -> Result<String> {
    Ok(serde_json::to_string(payload)?)
}
pub(super) fn decode(value: &str) -> Result<PublicationPayload> {
    let mut payload: PublicationPayload = serde_json::from_str(value)?;
    // Serde's nested Option normally collapses an explicit null into absence.
    // Preserve the legacy receipt distinction: absent = unknown, null = no remote branch.
    #[derive(serde::Deserialize)]
    struct RemoteTipPresence {
        #[serde(default, deserialize_with = "present_tip")]
        expected_remote_tip: Option<Option<String>>,
    }
    fn present_tip<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Option<Option<String>>, D::Error> {
        <Option<String> as serde::Deserialize>::deserialize(deserializer).map(Some)
    }
    payload.expected_remote_tip =
        serde_json::from_str::<RemoteTipPresence>(value)?.expected_remote_tip;
    Ok(payload)
}
pub(super) fn decode_identity(value: &str) -> Result<DocumentIdentity> {
    Ok(serde_json::from_str(value)?)
}
pub(super) fn rejected(payload: &PublicationPayload) -> PublicationPayload {
    let mut payload = payload.clone();
    payload.superseded_reason =
        Some("document compatibility or current project checks failed".into());
    payload
}
