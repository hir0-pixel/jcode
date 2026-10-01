//! One internal stop reason for provider safety-filter / refusal stops.

/// The internal stop reason every provider maps a blocked response to.
pub const REFUSAL_STOP_REASON: &str = "refusal";

/// True when a provider-native stop/finish reason means the response was
/// blocked or refused (Anthropic `refusal`, OpenAI `content_filter`, Gemini
/// `SAFETY`/`PROHIBITED_CONTENT`/`BLOCKLIST`/`SPII`, Bedrock `content_filtered`
/// /`guardrail_intervened`).
pub fn is_refusal_reason(reason: &str) -> bool {
    matches!(
        reason.trim().to_ascii_lowercase().as_str(),
        "refusal"
            | "content_filter"
            | "content_filtered"
            | "guardrail_intervened"
            | "safety"
            | "prohibited_content"
            | "blocklist"
            | "spii"
            | "image_safety"
    )
}

/// Map a provider stop reason to `refusal` when it is one, else keep it.
pub fn normalize_stop_reason(reason: String) -> String {
    if is_refusal_reason(&reason) {
        REFUSAL_STOP_REASON.to_string()
    } else {
        reason
    }
}

pub fn normalize_opt(reason: Option<String>) -> Option<String> {
    reason.map(normalize_stop_reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_every_provider_vocabulary() {
        for r in [
            "refusal", "content_filter", "content_filtered", "guardrail_intervened",
            "SAFETY", "PROHIBITED_CONTENT", "BLOCKLIST", "SPII",
        ] {
            assert_eq!(normalize_stop_reason(r.to_string()), "refusal", "{r}");
        }
        for r in ["end_turn", "tool_use", "max_tokens", "STOP", "length"] {
            assert_eq!(normalize_stop_reason(r.to_string()), r);
        }
    }
}
