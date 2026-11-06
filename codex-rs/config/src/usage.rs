//! Local usage-retention settings and duration validation.

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;

const MAX_RETENTION_DAYS: u32 = 36_500;

/// Independently configurable retention thresholds for local usage observations.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct UsageConfigToml {
    /// Days before raw usage observations are rolled up. Defaults to 7.
    #[serde(default, deserialize_with = "deserialize_retention_days")]
    #[schemars(range(min = 1, max = 36_500))]
    pub raw_retention_days: Option<u32>,

    /// Days of chat inactivity before retaining summary-only usage. Defaults to 7.
    #[serde(default, deserialize_with = "deserialize_retention_days")]
    #[schemars(range(min = 1, max = 36_500))]
    pub inactive_chat_days: Option<u32>,
}

impl UsageConfigToml {
    /// Validate values supplied programmatically as well as through TOML loading.
    pub fn validate(&self) -> Result<(), String> {
        for (name, days) in [
            ("raw_retention_days", self.raw_retention_days),
            ("inactive_chat_days", self.inactive_chat_days),
        ] {
            if let Some(days) = days {
                validate_days(days).map_err(|message| format!("usage.{name} {message}"))?;
            }
        }
        Ok(())
    }
}

fn validate_days(days: u32) -> Result<u32, String> {
    if !(1..=MAX_RETENTION_DAYS).contains(&days) {
        return Err(format!("must be between 1 and {MAX_RETENTION_DAYS} days"));
    }
    Ok(days)
}

fn deserialize_retention_days<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<u32>::deserialize(deserializer)?
        .map(validate_days)
        .transpose()
        .map_err(serde::de::Error::custom)
}
