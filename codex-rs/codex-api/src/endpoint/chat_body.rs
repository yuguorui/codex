//! Chat request-body policy: which reasoning effort a provider can read, and how
//! `extra_body` configuration is merged over it.
//!
//! Split out of `chat.rs`, which owns message and tool conversion, so the
//! provider-compatibility rules and the tests that pin them stay in one place.

use crate::common::Reasoning;
use crate::error::ApiError;
use codex_protocol::openai_models::ReasoningEffort;
use serde_json::Value;
use std::collections::HashMap;

/// Thinking-length field that Qwen3.8 rejects alongside `reasoning_effort`.
pub(super) const THINKING_BUDGET_KEY: &str = "thinking_budget";
/// OpenAI-style top-level reasoning strength field.
pub(super) const REASONING_EFFORT_KEY: &str = "reasoning_effort";
/// Streamed-usage field, which `extra_body` extends rather than replaces.
const STREAM_OPTIONS_KEY: &str = "stream_options";

/// Projects a configured reasoning effort onto the values chat providers accept.
///
/// Every major OpenAI-compatible chat endpoint takes a top-level `reasoning_effort`, but
/// the accepted vocabulary differs and out-of-vocabulary values are rejected rather than
/// ignored: Kimi K3 documents `low`/`high`/`max`, GLM-5.3 on the standard API accepts
/// only those three, Qwen3.8 accepts `xhigh`/`medium`/`low` and maps both `high` and
/// `max` onto `xhigh`, and DeepSeek V4 accepts `high`/`max` and maps `low` and `medium`
/// onto `high`. `low`/`high`/`max` is the largest set that is documented-safe everywhere.
///
/// The projection rounds upward rather than preserving the level exactly: Qwen3.8 maps
/// `high` onto `xhigh`, so a configured `medium` reaches it as that provider's top
/// thinking budget, and DeepSeek maps `low` onto `high`, so `minimal` arrives as `high`.
/// A model that needs the exact level declares the wire value in its catalog
/// `extra_body`, which `finalize_chat_body` merges over this projection.
///
/// Efforts with no chat-wire meaning are omitted rather than guessed:
///
/// - `None` and `Persistent` ask for reasoning to be off or carried across turns.
///   Neither is portable: Qwen3.8 spells "off" as `enable_thinking: false`, DeepSeek and
///   GLM spell it as `thinking: {"type": "disabled"}` — which GLM-5.3 rejects outright
///   because it always thinks — and Kimi K3 has no off switch at all. Provider defaults
///   already mean "thinking on", so omitting the field is the honest encoding, and a
///   model that wants the switch declares it in `extra_body`.
/// - `Custom` carries a wire value this client does not know, including the
///   Responses-only spelling `disabled` that `resolve_reasoning_effort` produces for
///   `Persistent`. Guessing a chat value for it would be a silent behavior change.
pub(super) fn chat_wire_reasoning_effort(reasoning: Option<&Reasoning>) -> Option<String> {
    match reasoning?.effort.as_ref()? {
        ReasoningEffort::Minimal | ReasoningEffort::Low => Some("low".to_string()),
        ReasoningEffort::Medium | ReasoningEffort::High | ReasoningEffort::XHigh => {
            Some("high".to_string())
        }
        ReasoningEffort::Max | ReasoningEffort::Ultra => Some("max".to_string()),
        ReasoningEffort::None | ReasoningEffort::Persistent | ReasoningEffort::Custom(_) => None,
    }
}

/// Merges both `extra_body` layers over a built request body, then reconciles the
/// thinking controls the merged result implies.
///
/// Provider configuration merges first and the model catalog second, so a model can
/// narrow or override what its provider declares. Reconciliation then drops a projected
/// `reasoning_effort` when the merged body would carry a real `thinking_budget`: Qwen3.8
/// rejects a request with both, and the budget already expresses the strength. An effort
/// declared in `extra_body` survives, because that pairing is then explicit
/// configuration and its owner is responsible for it.
///
/// Only the top-level `thinking_budget` key is reconciled. A provider that nests its
/// thinking controls elsewhere — vLLM's `chat_template_kwargs`, for one — is left exactly
/// as configured, including alongside the projected effort.
pub(super) fn finalize_chat_body(
    body: &mut Value,
    provider_extra_body: &HashMap<String, Value>,
    model_extra_body: &HashMap<String, Value>,
) -> Result<(), ApiError> {
    merge_extra_body(body, provider_extra_body)?;
    merge_extra_body(body, model_extra_body)?;
    // Judge the budget from the merged body, not from the declarations: a layer that
    // nulls the budget out has removed the conflict, and only a value that actually
    // reaches the wire can be rejected alongside the effort.
    let budget_on_the_wire = body
        .get(THINKING_BUDGET_KEY)
        .is_some_and(|value| !value.is_null());
    // The effort is only removed when this client projected it. A declared one is
    // explicit configuration, and a `null` declaration is not a declaration: it removes
    // the key rather than sending it.
    let effort_declared = [provider_extra_body, model_extra_body]
        .into_iter()
        .any(|extra_body| {
            extra_body
                .get(REASONING_EFFORT_KEY)
                .is_some_and(|value| !value.is_null())
        });
    if budget_on_the_wire
        && !effort_declared
        && let Some(body) = body.as_object_mut()
    {
        body.remove(REASONING_EFFORT_KEY);
    }
    Ok(())
}

fn merge_extra_body(body: &mut Value, extra_body: &HashMap<String, Value>) -> Result<(), ApiError> {
    if extra_body.is_empty() {
        return Ok(());
    }
    let Some(body) = body.as_object_mut() else {
        return Ok(());
    };
    for (key, value) in extra_body {
        if key == STREAM_OPTIONS_KEY {
            let Some(base_stream_options) = body
                .get_mut(STREAM_OPTIONS_KEY)
                .and_then(Value::as_object_mut)
            else {
                continue;
            };
            let Some(extra_stream_options) = value.as_object() else {
                return Err(ApiError::Stream(
                    "extra_body.stream_options must be an object".to_string(),
                ));
            };
            for (option_key, option_value) in extra_stream_options {
                base_stream_options.insert(option_key.clone(), option_value.clone());
            }
            base_stream_options.insert("include_usage".to_string(), Value::Bool(true));
        } else if value.is_null() && body.contains_key(key) {
            // `null` removes a field this client already put in the body, which is how a
            // model opts out of a default it does not support — a non-reasoning model
            // dropping the projected `reasoning_effort`, for example — without a code
            // change. Any other null is sent as JSON null, because a provider may require
            // the key to be present with that value. `stream_options` is handled above,
            // so it can never be removed here. Only a JSON configuration can express the
            // opt-out: a model catalog, or a remote thread config's `extra_body_json`.
            // `config.toml` cannot, because TOML has no null.
            body.remove(key);
        } else {
            body.insert(key.clone(), value.clone());
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "chat_body_tests.rs"]
mod tests;
