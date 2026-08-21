use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TargetToken(usize);

impl TargetToken {
    pub fn from_platform_value(value: usize) -> Self {
        Self(value)
    }

    pub fn platform_value(self) -> usize {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PasteFallbackReason {
    UnsafeTarget,
    TargetUnavailable,
    UnsupportedContent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PasteOutcome {
    CommandSent,
    CopyOnly { reason: PasteFallbackReason },
}

impl PasteOutcome {
    pub fn user_message(self) -> &'static str {
        match self {
            Self::CommandSent => "Paste command sent",
            Self::CopyOnly { .. } => "Cannot paste safely; content was copied. Paste it manually.",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PasteFallbackReason, PasteOutcome};
    use serde::{Serialize, de::DeserializeOwned};
    use static_assertions::assert_not_impl_any;

    assert_not_impl_any!(super::TargetToken: Serialize, DeserializeOwned);

    #[test]
    fn paste_outcome_messages_do_not_claim_content_was_inserted() {
        assert_eq!(
            PasteOutcome::CommandSent.user_message(),
            "Paste command sent"
        );
        assert_eq!(
            PasteOutcome::CopyOnly {
                reason: PasteFallbackReason::UnsafeTarget,
            }
            .user_message(),
            "Cannot paste safely; content was copied. Paste it manually."
        );
    }

    #[test]
    fn paste_outcome_uses_stable_snake_case_json_tags() {
        let outcome = PasteOutcome::CopyOnly {
            reason: PasteFallbackReason::TargetUnavailable,
        };

        let json = serde_json::to_string(&outcome).unwrap();

        assert_eq!(
            json,
            r#"{"status":"copy_only","reason":"target_unavailable"}"#
        );
        assert_eq!(
            serde_json::from_str::<PasteOutcome>(&json).unwrap(),
            outcome
        );
    }
}
