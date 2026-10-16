use clap::Parser;
use codex_core::config::ConfigBuilder;
use codex_state::network_diagnostics::NetworkQuery;
use codex_utils_cli::CliConfigOverrides;

#[derive(Debug, Parser)]
pub(crate) struct NetworkDiagnosticsArgs {
    /// Include successful requests and completion markers around the incidents.
    #[arg(long)]
    include_context: bool,
    /// Restrict incidents to one task.
    #[arg(long)]
    thread_id: Option<String>,
    /// Include records at or after this Unix timestamp in milliseconds.
    #[arg(long)]
    since_ms: Option<i64>,
    /// Continue an older page using nextBeforeId from the previous result.
    #[arg(long)]
    before_id: Option<i64>,
    /// Maximum records, newest first. Records are retained without automatic expiry.
    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..=100))]
    limit: u32,
}

pub(crate) async fn run(
    args: NetworkDiagnosticsArgs,
    overrides: CliConfigOverrides,
) -> anyhow::Result<()> {
    let config = ConfigBuilder::default()
        .cli_overrides(overrides.parse_overrides().map_err(anyhow::Error::msg)?)
        .build()
        .await?;
    let page = codex_state::network_diagnostics::query(
        config.sqlite_config(),
        NetworkQuery {
            incidents_only: !args.include_context,
            thread_id: args.thread_id,
            since_ms: args.since_ms,
            before_id: args.before_id,
            limit: args.limit,
        },
    )
    .await?;
    serde_json::to_writer(std::io::stdout(), &page)?;
    println!();
    Ok(())
}
