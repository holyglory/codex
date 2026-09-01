use super::error::UsageCommandError;
use super::error::UsageErrorKind;
use chrono::DateTime;
use clap::Args;
use codex_usage::Activity;
use codex_usage::AttributionProvenance;
use codex_usage::CoverageState;
use codex_usage::Phase;
use codex_usage::RepositoryId;
use codex_usage::TerminalStatus;
use codex_usage::ThreadId;
use codex_usage::UsageActivityRecord;
use codex_usage::UsageEventRecord;
use codex_usage::UsagePageCursor;
use codex_usage::UsagePageRequest;
use codex_usage::UsageToolRecord;
use codex_usage::UtcTimeRange;
use std::collections::BTreeMap;

const DEFAULT_PAGE_LIMIT: u32 = 100;
const MAX_PAGE_LIMIT: u32 = 500;

#[derive(Clone, Debug, Default, Args)]
pub(super) struct UsageFilters {
    #[arg(long, global = true)]
    pub(super) account: Option<String>,
    #[arg(long, global = true)]
    pub(super) model: Option<String>,
    #[arg(long, global = true)]
    pub(super) agent: Option<String>,
    #[arg(long, global = true, value_parser = parse_phase)]
    pub(super) phase: Option<Phase>,
    #[arg(long, global = true, value_parser = parse_activity)]
    pub(super) activity: Option<Activity>,
    #[arg(long, global = true)]
    pub(super) tool: Option<String>,
    #[arg(long, visible_alias = "repo", global = true)]
    pub(super) repository: Option<String>,
    #[arg(long, global = true)]
    pub(super) thread: Option<String>,
    #[arg(long, global = true)]
    pub(super) turn: Option<String>,
    #[arg(long, global = true)]
    pub(super) status: Option<String>,
    #[arg(long, global = true)]
    pub(super) provenance: Option<String>,
    #[arg(long, global = true)]
    pub(super) coverage: Option<String>,
    /// Inclusive UTC RFC3339 timestamp or Unix milliseconds.
    #[arg(long, global = true)]
    pub(super) since: Option<String>,
    /// Exclusive UTC RFC3339 timestamp or Unix milliseconds.
    #[arg(long, global = true)]
    pub(super) until: Option<String>,
    #[arg(long, global = true, value_name = "DIMENSION")]
    pub(super) breakdown: Vec<String>,
}

#[derive(Clone, Debug, Args)]
pub(super) struct PageArgs {
    #[arg(long, default_value_t = DEFAULT_PAGE_LIMIT, value_parser = clap::value_parser!(u32).range(1..=i64::from(MAX_PAGE_LIMIT)))]
    limit: u32,
    #[arg(long)]
    cursor: Option<String>,
}

impl UsageFilters {
    pub(super) fn ensure_only(
        &self,
        allowed: &[&str],
        allowed_breakdowns: &[&str],
    ) -> Result<(), UsageCommandError> {
        let present = [
            ("account", self.account.is_some()),
            ("model", self.model.is_some()),
            ("agent", self.agent.is_some()),
            ("phase", self.phase.is_some()),
            ("activity", self.activity.is_some()),
            ("tool", self.tool.is_some()),
            ("repository", self.repository.is_some()),
            ("thread", self.thread.is_some()),
            ("turn", self.turn.is_some()),
            ("status", self.status.is_some()),
            ("provenance", self.provenance.is_some()),
            ("coverage", self.coverage.is_some()),
            ("since", self.since.is_some()),
            ("until", self.until.is_some()),
        ];
        if present
            .into_iter()
            .any(|(name, present)| present && !allowed.contains(&name))
            || self
                .breakdown
                .iter()
                .any(|dimension| !allowed_breakdowns.contains(&dimension.as_str()))
        {
            return Err(UsageCommandError::new(UsageErrorKind::Input));
        }
        Ok(())
    }

    pub(super) fn time_range(&self) -> Result<Option<UtcTimeRange>, UsageCommandError> {
        if self.since.is_none() && self.until.is_none() {
            return Ok(None);
        }
        let start = self
            .since
            .as_deref()
            .map(parse_timestamp)
            .transpose()?
            .unwrap_or(i64::MIN);
        let end = self
            .until
            .as_deref()
            .map(parse_timestamp)
            .transpose()?
            .unwrap_or(i64::MAX);
        UtcTimeRange::new(start, end)
            .map(Some)
            .map_err(|_| UsageCommandError::new(UsageErrorKind::Input))
    }

    pub(super) fn thread_id(&self) -> Result<Option<ThreadId>, UsageCommandError> {
        self.thread
            .as_ref()
            .map(ThreadId::new)
            .transpose()
            .map_err(|_| UsageCommandError::new(UsageErrorKind::Input))
    }

    pub(super) fn phase_value(&self) -> Result<Option<Phase>, UsageCommandError> {
        Ok(self.phase)
    }

    pub(super) fn activity_value(&self) -> Result<Option<Activity>, UsageCommandError> {
        Ok(self.activity)
    }

    pub(super) fn terminal_status(&self) -> Result<Option<TerminalStatus>, UsageCommandError> {
        self.status
            .as_deref()
            .map(|value| {
                TerminalStatus::parse(value)
                    .ok_or_else(|| UsageCommandError::new(UsageErrorKind::Input))
            })
            .transpose()
    }

    pub(super) fn attribution_provenance(
        &self,
    ) -> Result<Option<AttributionProvenance>, UsageCommandError> {
        self.provenance
            .as_deref()
            .map(|value| {
                AttributionProvenance::parse(value)
                    .ok_or_else(|| UsageCommandError::new(UsageErrorKind::Input))
            })
            .transpose()
    }

    pub(super) fn event_provenance(
        &self,
    ) -> Result<Option<codex_usage::UsageEventProvenance>, UsageCommandError> {
        self.provenance
            .as_deref()
            .map(|value| {
                codex_usage::UsageEventProvenance::parse(value)
                    .ok_or_else(|| UsageCommandError::new(UsageErrorKind::Input))
            })
            .transpose()
    }

    pub(super) fn coverage_state(&self) -> Result<Option<CoverageState>, UsageCommandError> {
        self.coverage
            .as_deref()
            .map(|value| {
                CoverageState::parse(value)
                    .ok_or_else(|| UsageCommandError::new(UsageErrorKind::Input))
            })
            .transpose()
    }

    pub(super) fn cursor_kind(&self, kind: &str) -> String {
        let input = format!(
            "{kind}\0{:?}\0{:?}\0{:?}\0{:?}\0{:?}\0{:?}\0{:?}\0{:?}\0{:?}\0{:?}\0{:?}",
            self.agent,
            self.phase,
            self.activity,
            self.tool,
            self.repository,
            self.thread,
            self.status,
            self.provenance,
            self.since,
            self.until,
            self.breakdown,
        );
        format!("{kind}-{:016x}", fnv1a(input.as_bytes()))
    }
}

pub(super) fn page_request(
    page: &PageArgs,
    kind: &str,
) -> Result<UsagePageRequest, UsageCommandError> {
    Ok(UsagePageRequest {
        cursor: page
            .cursor
            .as_deref()
            .map(|cursor| decode_cursor(kind, cursor))
            .transpose()?,
        limit: page.limit,
    })
}

pub(super) fn encode_cursor(kind: &str, cursor: &UsagePageCursor) -> String {
    hex_encode(format!("v1\0{kind}\0{}\0{}", cursor.occurred_at_ms(), cursor.id()).as_bytes())
}

pub(super) fn decode_cursor(kind: &str, value: &str) -> Result<UsagePageCursor, UsageCommandError> {
    let bytes = hex_decode(value)?;
    let text =
        String::from_utf8(bytes).map_err(|_| UsageCommandError::new(UsageErrorKind::Input))?;
    let mut fields = text.split('\0');
    let valid = fields.next() == Some("v1") && fields.next() == Some(kind);
    let occurred_at_ms = fields.next().and_then(|value| value.parse::<i64>().ok());
    let id = fields.next();
    if !valid || fields.next().is_some() {
        return Err(UsageCommandError::new(UsageErrorKind::Input));
    }
    UsagePageCursor::new(
        occurred_at_ms.ok_or_else(|| UsageCommandError::new(UsageErrorKind::Input))?,
        id.ok_or_else(|| UsageCommandError::new(UsageErrorKind::Input))?,
    )
    .ok_or_else(|| UsageCommandError::new(UsageErrorKind::Input))
}

pub(super) fn breakdown_tools(
    data: &[UsageToolRecord],
    dimensions: &[String],
) -> BTreeMap<String, u64> {
    breakdown(
        dimensions,
        data.iter().map(|record| {
            dimensions
                .iter()
                .map(|dimension| match dimension.as_str() {
                    "repository" => record
                        .repository_id
                        .as_ref()
                        .map_or("unknown", RepositoryId::as_str),
                    "thread" => record.thread_id.as_str(),
                    "tool" => record.tool_name.as_str(),
                    "status" => record.status.map_or("unknown", TerminalStatus::as_str),
                    "provenance" => record.provenance.as_str(),
                    _ => "unsupported",
                })
                .collect::<Vec<_>>()
                .join("/")
        }),
    )
}

pub(super) fn breakdown_activities(
    data: &[UsageActivityRecord],
    dimensions: &[String],
) -> BTreeMap<String, u64> {
    breakdown(
        dimensions,
        data.iter().map(|record| {
            dimensions
                .iter()
                .map(|dimension| match dimension.as_str() {
                    "agent" => record.agent_id.as_str(),
                    "thread" => record.thread_id.as_str(),
                    "phase" => record.phase.as_str(),
                    "activity" => record.activity.as_str(),
                    "provenance" => record.provenance.as_str(),
                    _ => "unsupported",
                })
                .collect::<Vec<_>>()
                .join("/")
        }),
    )
}

pub(super) fn breakdown_events(
    data: &[UsageEventRecord],
    dimensions: &[String],
) -> BTreeMap<String, u64> {
    breakdown(
        dimensions,
        data.iter().map(|record| {
            dimensions
                .iter()
                .map(|dimension| match dimension.as_str() {
                    "repository" => record
                        .repository_id
                        .as_ref()
                        .map_or("unknown", RepositoryId::as_str),
                    "thread" => record
                        .thread_id
                        .as_ref()
                        .map_or("unknown", ThreadId::as_str),
                    "provenance" => record.provenance.as_str(),
                    "coverage" => record.coverage.as_str(),
                    _ => "unsupported",
                })
                .collect::<Vec<_>>()
                .join("/")
        }),
    )
}

pub(super) fn combine_fixed<T: Eq>(
    fixed: Option<T>,
    filtered: Option<T>,
) -> Result<Option<T>, UsageCommandError> {
    match (fixed, filtered) {
        (Some(fixed), Some(filtered)) if fixed != filtered => {
            Err(UsageCommandError::new(UsageErrorKind::Input))
        }
        (Some(fixed), _) => Ok(Some(fixed)),
        (_, filtered) => Ok(filtered),
    }
}

pub(super) fn parse_phase(value: &str) -> Result<Phase, UsageCommandError> {
    Phase::parse(value).ok_or_else(|| UsageCommandError::new(UsageErrorKind::Input))
}

pub(super) fn parse_activity(value: &str) -> Result<Activity, UsageCommandError> {
    Activity::parse(value).ok_or_else(|| UsageCommandError::new(UsageErrorKind::Input))
}

fn parse_timestamp(value: &str) -> Result<i64, UsageCommandError> {
    value.parse::<i64>().or_else(|_| {
        DateTime::parse_from_rfc3339(value)
            .map(|timestamp| timestamp.timestamp_millis())
            .map_err(|_| UsageCommandError::new(UsageErrorKind::Input))
    })
}

fn breakdown(
    dimensions: &[String],
    keys: impl IntoIterator<Item = String>,
) -> BTreeMap<String, u64> {
    let mut result = BTreeMap::new();
    if dimensions.is_empty() {
        return result;
    }
    for key in keys {
        *result.entry(key).or_default() += 1;
    }
    result
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn hex_decode(value: &str) -> Result<Vec<u8>, UsageCommandError> {
    if !value.len().is_multiple_of(2) || value.len() > 1_024 {
        return Err(UsageCommandError::new(UsageErrorKind::Input));
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = hex_digit(pair[0])?;
            let low = hex_digit(pair[1])?;
            Ok((high << 4) | low)
        })
        .collect()
}

fn hex_digit(value: u8) -> Result<u8, UsageCommandError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(UsageCommandError::new(UsageErrorKind::Input)),
    }
}
