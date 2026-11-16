use duckdb::Config;
use duckdb::Connection;

/// Foreground reads receive independent, bounded query-engine resources.
pub(crate) enum ReadBudget {
    Report,
    TokenHeadline,
}

/// Apply the read budget before DuckDB initializes its worker pool and buffers.
/// The memory bound covers DuckDB-managed memory; reads never spill to disk.
pub(crate) fn open_read_in_memory(budget: ReadBudget) -> duckdb::Result<Connection> {
    let (threads, memory) = match budget {
        ReadBudget::Report => (2, "512MiB"),
        ReadBudget::TokenHeadline => (1, "64MiB"),
    };
    let config = Config::default()
        .enable_autoload_extension(/*enabled*/ false)?
        .threads(threads)?
        .max_memory(memory)?
        .with("temp_directory", "")?
        .with("max_temp_directory_size", "0B")?;
    Connection::open_in_memory_with_flags(config)
}

/// Use the bundled JSON and Parquet support without downloading extensions.
pub(crate) fn open_in_memory() -> duckdb::Result<Connection> {
    let config = Config::default().enable_autoload_extension(/*enabled*/ false)?;
    // Keep external access available for the local immutable Parquet files.
    Connection::open_in_memory_with_flags(config)
}
