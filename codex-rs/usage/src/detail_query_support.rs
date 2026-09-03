use crate::AccountProfileRef;
use crate::AgentId;
use crate::ModelRequestId;
use crate::OperationId;
use crate::ThreadId;
use crate::ToolInvocationId;
use crate::TurnId;
use crate::UsageDetailListQuery;
use crate::UsageDetailRecord;
use crate::UsagePage;
use crate::UsagePageCursor;
use crate::UsageStoreError;
use sqlx::Row;

pub(crate) fn cursor_at(query: &UsageDetailListQuery) -> Option<i64> {
    query
        .page
        .cursor
        .as_ref()
        .map(UsagePageCursor::occurred_at_ms)
}

pub(crate) fn cursor_id(query: &UsageDetailListQuery) -> Option<&str> {
    query.page.cursor.as_ref().map(UsagePageCursor::id)
}

pub(crate) fn fetch_limit(query: &UsageDetailListQuery) -> Result<i64, UsageStoreError> {
    i64::from(query.page.limit)
        .checked_add(1)
        .ok_or(UsageStoreError::DatabaseValueOutOfRange)
}

pub(crate) fn page_from_rows(
    rows: &[sqlx::sqlite::SqliteRow],
    query: &UsageDetailListQuery,
    timestamp_column: &str,
    id_column: &str,
    map: impl Fn(&sqlx::sqlite::SqliteRow) -> Result<UsageDetailRecord, UsageStoreError>,
) -> Result<UsagePage<UsageDetailRecord>, UsageStoreError> {
    let data = rows
        .iter()
        .take(query.page.limit as usize)
        .map(map)
        .collect::<Result<Vec<_>, _>>()?;
    let next_cursor = if rows.len() > query.page.limit as usize {
        let row = rows
            .get(query.page.limit.saturating_sub(1) as usize)
            .ok_or(UsageStoreError::DatabaseValueOutOfRange)?;
        Some(
            UsagePageCursor::new(row.get(timestamp_column), row.get::<String, _>(id_column))
                .ok_or(UsageStoreError::InvalidFact)?,
        )
    } else {
        None
    };
    Ok(UsagePage { data, next_cursor })
}

pub(crate) fn uuid(value: String) -> Result<String, UsageStoreError> {
    uuid::Uuid::parse_str(&value)
        .map(|_| value)
        .map_err(|_| UsageStoreError::InvalidFact)
}

pub(crate) fn optional_uuid(value: Option<String>) -> Result<Option<String>, UsageStoreError> {
    value.map(uuid).transpose()
}

pub(crate) fn thread(value: String) -> Result<String, UsageStoreError> {
    ThreadId::new(value)
        .map(|id| id.as_str().to_string())
        .map_err(|_| UsageStoreError::InvalidFact)
}

pub(crate) fn turn(value: String) -> Result<String, UsageStoreError> {
    TurnId::new(value)
        .map(|id| id.as_str().to_string())
        .map_err(|_| UsageStoreError::InvalidFact)
}

pub(crate) fn agent(value: String) -> Result<String, UsageStoreError> {
    AgentId::new(value)
        .map(|id| id.as_str().to_string())
        .map_err(|_| UsageStoreError::InvalidFact)
}

pub(crate) fn operation(value: String) -> Result<String, UsageStoreError> {
    OperationId::from_string(&value)
        .map(OperationId::as_string)
        .ok_or(UsageStoreError::InvalidFact)
}

pub(crate) fn model_request(value: String) -> Result<String, UsageStoreError> {
    ModelRequestId::from_string(&value)
        .map(ModelRequestId::as_string)
        .ok_or(UsageStoreError::InvalidFact)
}

pub(crate) fn tool_invocation(value: String) -> Result<String, UsageStoreError> {
    ToolInvocationId::from_string(&value)
        .map(ToolInvocationId::as_string)
        .ok_or(UsageStoreError::InvalidFact)
}

pub(crate) fn display_account(
    value: Option<String>,
    account_label: impl Fn(&AccountProfileRef) -> String,
) -> Result<Option<String>, UsageStoreError> {
    value
        .map(|value| {
            AccountProfileRef::new(value)
                .map(|reference| account_label(&reference))
                .map_err(|_| UsageStoreError::InvalidFact)
        })
        .transpose()
}

pub(crate) fn required_enum<T: Copy>(
    value: String,
    parse: impl Fn(&str) -> Option<T>,
    display: impl Fn(T) -> &'static str,
) -> Result<String, UsageStoreError> {
    parse(&value)
        .map(display)
        .map(str::to_string)
        .ok_or(UsageStoreError::InvalidFact)
}

pub(crate) fn optional_enum<T: Copy>(
    value: Option<String>,
    parse: impl Fn(&str) -> Option<T>,
    display: impl Fn(T) -> &'static str,
) -> Result<Option<String>, UsageStoreError> {
    value
        .map(|value| required_enum(value, &parse, &display))
        .transpose()
}

pub(crate) fn positive(value: i64) -> Result<i64, UsageStoreError> {
    (value > 0)
        .then_some(value)
        .ok_or(UsageStoreError::InvalidFact)
}
