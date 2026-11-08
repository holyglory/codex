use duckdb::Config;
use duckdb::Connection;

/// Use the bundled JSON and Parquet support without downloading extensions.
pub(crate) fn open_in_memory() -> duckdb::Result<Connection> {
    let config = Config::default().enable_autoload_extension(/*enabled*/ false)?;
    // Keep external access available for the local immutable Parquet files.
    Connection::open_in_memory_with_flags(config)
}
