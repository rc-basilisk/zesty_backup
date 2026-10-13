//! Create a backup and upload it, using zesty-backup as a library.
//!
//! Usage: cargo run --example basic-backup -- path/to/config.toml

use anyhow::Result;
use zesty_backup::{backup, config::AppConfig, providers};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("zesty_backup=info")
        .init();

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.toml".into());
    let config = AppConfig::load(&config_path)?;

    let archive = backup::create_backup(&config, false)?;
    println!("Created {}", archive.display());

    let provider = providers::from_config(&config.storage).await?;
    backup::upload(provider.as_ref(), &config, Some(&archive)).await?;
    println!("Uploaded. Remote backups:");
    for item in backup::remote_backups(provider.as_ref()).await? {
        println!("  {} ({})", item.key, backup::human_size(item.size));
    }
    Ok(())
}
