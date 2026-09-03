//! Content-free, presence-preserving usage values reported by model and media providers.

use serde::Deserialize;
use serde::Deserializer;
use serde_json::Map;
use serde_json::Value;
use sha2::Digest as _;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::fmt;

/// Version of the validated category-key and count-presence contract.
pub const PROVIDER_USAGE_CATEGORY_SCHEMA_VERSION: u16 = 1;

const MAX_ADDITIONAL_TOKEN_CATEGORIES: usize = 64;
const MAX_CATEGORY_PATH_BYTES: usize = 192;
const MAX_CATEGORY_PATH_DEPTH: usize = 4;
const MAX_CATEGORY_SEGMENT_BYTES: usize = 64;
const MAX_HIDDEN_SCAN_DEPTH: usize = 16;
const MAX_USAGE_TRAVERSAL_NODES: usize = 512;

const INPUT_TOKENS_PATH: &str = "input_tokens";
const CACHED_INPUT_TOKENS_PATH: &str = "input_tokens_details.cached_tokens";
const CACHE_WRITE_INPUT_TOKENS_PATH: &str = "input_tokens_details.cache_write_tokens";
const OUTPUT_TOKENS_PATH: &str = "output_tokens";
const REASONING_OUTPUT_TOKENS_PATH: &str = "output_tokens_details.reasoning_tokens";
const TOTAL_TOKENS_PATH: &str = "total_tokens";

/// Exact presence and validity of one provider token count.
///
/// Values use the complete non-negative JSON integer range (`u64`). Conversion into legacy signed
/// counters is deliberately left to compatibility projections so this DTO does not discard an
/// otherwise valid provider-native value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ProviderTokenCount {
    /// The provider did not include the category.
    #[default]
    Absent,
    /// The provider included the category with an explicit JSON null.
    Null,
    /// The provider included a non-negative integer.
    Value(u64),
    /// The provider included the category with an unsupported value shape.
    Invalid,
}

/// Presence-preserving token usage reported by a model or media provider.
///
/// Known categories retain explicit absent, null, value, and invalid states. Future numeric
/// categories use version-1 lowercase snake-case paths and a bounded traversal/retention policy.
/// No response text, tool content, raw provider identifiers, or other payload data is retained.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderUsage {
    input_tokens: ProviderTokenCount,
    cached_input_tokens: ProviderTokenCount,
    cache_write_input_tokens: ProviderTokenCount,
    output_tokens: ProviderTokenCount,
    reasoning_output_tokens: ProviderTokenCount,
    total_tokens: ProviderTokenCount,
    additional_token_categories: BTreeMap<String, ProviderTokenCount>,
    categories_complete: bool,
}

impl ProviderUsage {
    /// Category-key contract used by [`Self::additional_token_categories`].
    pub fn category_schema_version(&self) -> u16 {
        PROVIDER_USAGE_CATEGORY_SCHEMA_VERSION
    }

    /// Provider input-token count and its exact presence state.
    pub fn input_tokens(&self) -> ProviderTokenCount {
        self.input_tokens
    }

    /// Provider cached-input-token count and its exact presence state.
    pub fn cached_input_tokens(&self) -> ProviderTokenCount {
        self.cached_input_tokens
    }

    /// Provider cache-write-input-token count and its exact presence state.
    pub fn cache_write_input_tokens(&self) -> ProviderTokenCount {
        self.cache_write_input_tokens
    }

    /// Provider output-token count and its exact presence state.
    pub fn output_tokens(&self) -> ProviderTokenCount {
        self.output_tokens
    }

    /// Provider reasoning-output-token count and its exact presence state.
    pub fn reasoning_output_tokens(&self) -> ProviderTokenCount {
        self.reasoning_output_tokens
    }

    /// Provider total-token count and its exact presence state.
    pub fn total_tokens(&self) -> ProviderTokenCount {
        self.total_tokens
    }

    /// Future token-shaped categories that passed the versioned safe-key policy.
    pub fn additional_token_categories(&self) -> &BTreeMap<String, ProviderTokenCount> {
        &self.additional_token_categories
    }

    /// Whether every token-shaped category was inspected and represented under the bounded policy.
    pub fn categories_complete(&self) -> bool {
        self.categories_complete
    }

    /// Parses only the provider usage object into bounded content-free metadata.
    pub fn from_json_value(value: &Value) -> Self {
        let Some(root) = value.as_object() else {
            return Self {
                categories_complete: false,
                ..Self::default()
            };
        };

        let (input_tokens, input_tokens_valid) = known_count(root, INPUT_TOKENS_PATH);
        let (cached_input_tokens, cached_input_tokens_valid) =
            known_count(root, CACHED_INPUT_TOKENS_PATH);
        let (cache_write_input_tokens, cache_write_input_tokens_valid) =
            known_count(root, CACHE_WRITE_INPUT_TOKENS_PATH);
        let (output_tokens, output_tokens_valid) = known_count(root, OUTPUT_TOKENS_PATH);
        let (reasoning_output_tokens, reasoning_output_tokens_valid) =
            known_count(root, REASONING_OUTPUT_TOKENS_PATH);
        let (total_tokens, total_tokens_valid) = known_count(root, TOTAL_TOKENS_PATH);

        let mut additional_token_categories = BTreeMap::new();
        let mut categories_complete = input_tokens_valid
            && cached_input_tokens_valid
            && cache_write_input_tokens_valid
            && output_tokens_valid
            && reasoning_output_tokens_valid
            && total_tokens_valid;
        let mut budget = TraversalBudget::default();
        collect_additional_counts(
            root,
            &mut Vec::new(),
            &mut additional_token_categories,
            &mut categories_complete,
            &mut budget,
        );

        Self {
            input_tokens,
            cached_input_tokens,
            cache_write_input_tokens,
            output_tokens,
            reasoning_output_tokens,
            total_tokens,
            additional_token_categories,
            categories_complete,
        }
    }
}

impl<'de> Deserialize<'de> for ProviderUsage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        Ok(Self::from_json_value(&value))
    }
}

/// Stable content-free fingerprint of a provider response identity.
///
/// The raw provider ID is hashed immediately with a versioned domain separator and is never held
/// by this type. The fingerprint can be persisted for replay reconciliation; its `Debug` output is
/// intentionally redacted to keep it out of routine logs.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ProviderSourceEventKey([u8; 32]);

impl ProviderSourceEventKey {
    /// Derives a stable versioned fingerprint without retaining the provider's raw response ID.
    pub fn from_provider_response_id(response_id: &str) -> Option<Self> {
        if response_id.is_empty() {
            return None;
        }
        let mut digest = Sha256::new();
        digest.update(b"codex-provider-response-id-v1\0");
        digest.update(response_id.as_bytes());
        Some(Self(digest.finalize().into()))
    }

    /// Safe fingerprint bytes suitable for equality checks and durable deduplication.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for ProviderSourceEventKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProviderSourceEventKey([redacted])")
    }
}

fn known_count(root: &Map<String, Value>, path: &str) -> (ProviderTokenCount, bool) {
    let mut segments = path.split('.');
    let Some(first) = segments.next() else {
        return (ProviderTokenCount::Invalid, false);
    };
    let Some(mut value) = root.get(first) else {
        return (ProviderTokenCount::Absent, true);
    };
    for segment in segments {
        let object = match value {
            Value::Null => return (ProviderTokenCount::Absent, true),
            Value::Object(object) => object,
            _ => return (ProviderTokenCount::Invalid, false),
        };
        let Some(next) = object.get(segment) else {
            return (ProviderTokenCount::Absent, true);
        };
        value = next;
    }

    match value {
        Value::Null => (ProviderTokenCount::Null, true),
        Value::Number(number) => match number.as_u64() {
            Some(count) => (ProviderTokenCount::Value(count), true),
            None => (ProviderTokenCount::Invalid, false),
        },
        _ => (ProviderTokenCount::Invalid, false),
    }
}

#[derive(Debug)]
struct TraversalBudget {
    remaining: usize,
}

impl Default for TraversalBudget {
    fn default() -> Self {
        Self {
            remaining: MAX_USAGE_TRAVERSAL_NODES,
        }
    }
}

impl TraversalBudget {
    fn visit(&mut self) -> bool {
        let Some(remaining) = self.remaining.checked_sub(1) else {
            return false;
        };
        self.remaining = remaining;
        true
    }
}

fn collect_additional_counts(
    object: &Map<String, Value>,
    path: &mut Vec<String>,
    categories: &mut BTreeMap<String, ProviderTokenCount>,
    complete: &mut bool,
    budget: &mut TraversalBudget,
) {
    for (segment, value) in object {
        if !budget.visit() {
            *complete = false;
            return;
        }
        // `attribution.items` is keyed by opaque response-item IDs. Those keys are neither token
        // category names nor safe stable identifiers, and the top-level usage object already
        // carries the provider-native totals. Do not retain or traverse this dynamic breakdown.
        if path.as_slice() == ["attribution"] && segment == "items" {
            continue;
        }
        let token_shaped = token_category_segment(segment);
        if !valid_category_segment(segment) {
            if token_shaped
                || hidden_subtree_contains_tokens(value, budget, /*depth*/ 0) != HiddenTokens::No
            {
                *complete = false;
            }
            continue;
        }

        let path_bytes = path.iter().map(String::len).sum::<usize>() + path.len() + segment.len();
        if path.len() >= MAX_CATEGORY_PATH_DEPTH || path_bytes > MAX_CATEGORY_PATH_BYTES {
            if token_shaped
                || hidden_subtree_contains_tokens(value, budget, /*depth*/ 0) != HiddenTokens::No
            {
                *complete = false;
            }
            continue;
        }

        path.push(segment.clone());
        let joined_path = path.join(".");
        match value {
            Value::Object(nested) if !token_shaped => {
                collect_additional_counts(nested, path, categories, complete, budget);
            }
            _ if token_shaped && !known_category_path(&joined_path) => {
                let presence = match value {
                    Value::Null => ProviderTokenCount::Null,
                    Value::Number(number) => number
                        .as_u64()
                        .map(ProviderTokenCount::Value)
                        .unwrap_or(ProviderTokenCount::Invalid),
                    _ => ProviderTokenCount::Invalid,
                };
                if presence == ProviderTokenCount::Invalid {
                    *complete = false;
                }
                if categories.len() >= MAX_ADDITIONAL_TOKEN_CATEGORIES {
                    *complete = false;
                } else {
                    categories.insert(joined_path, presence);
                }
            }
            _ => {}
        }
        path.pop();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HiddenTokens {
    Yes,
    No,
    Unknown,
}

fn hidden_subtree_contains_tokens(
    value: &Value,
    budget: &mut TraversalBudget,
    depth: usize,
) -> HiddenTokens {
    let Value::Object(object) = value else {
        return HiddenTokens::No;
    };
    if depth >= MAX_HIDDEN_SCAN_DEPTH {
        return HiddenTokens::Unknown;
    }
    for (segment, nested) in object {
        if !budget.visit() {
            return HiddenTokens::Unknown;
        }
        if token_category_segment(segment) {
            return HiddenTokens::Yes;
        }
        match hidden_subtree_contains_tokens(nested, budget, depth + 1) {
            HiddenTokens::Yes => return HiddenTokens::Yes,
            HiddenTokens::Unknown => return HiddenTokens::Unknown,
            HiddenTokens::No => {}
        }
    }
    HiddenTokens::No
}

fn valid_category_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= MAX_CATEGORY_SEGMENT_BYTES
        && segment.bytes().enumerate().all(|(index, byte)| match byte {
            b'a'..=b'z' => true,
            b'0'..=b'9' | b'_' => index > 0,
            _ => false,
        })
}

fn token_category_segment(segment: &str) -> bool {
    segment == "tokens" || segment.ends_with("_tokens")
}

fn known_category_path(path: &str) -> bool {
    matches!(
        path,
        INPUT_TOKENS_PATH
            | CACHED_INPUT_TOKENS_PATH
            | CACHE_WRITE_INPUT_TOKENS_PATH
            | OUTPUT_TOKENS_PATH
            | REASONING_OUTPUT_TOKENS_PATH
            | TOTAL_TOKENS_PATH
    )
}

#[cfg(test)]
#[path = "provider_usage_tests.rs"]
mod tests;
