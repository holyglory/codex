//! Apply the store's normal migrations and derived indexes to an owned usage home.

use std::path::PathBuf;

#[tokio::main]
async fn main() {
    let Some(home) = std::env::args_os().nth(1).map(PathBuf::from) else {
        eprintln!("usage: refresh_query_indexes <codex-home>");
        std::process::exit(2);
    };
    match codex_usage::UsageStore::open(&home).await {
        Ok(_) => println!("usage query indexes ready"),
        Err(_) => {
            eprintln!("usage query index refresh failed");
            std::process::exit(1);
        }
    }
}
