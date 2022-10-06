use anyhow::Result;
use tas_rust::{setup, Config};

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::default();
    let _ = setup(config).await?;
    Ok(())
}
