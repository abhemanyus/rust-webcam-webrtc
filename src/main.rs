use anyhow::Result;
use tas_rust::{setup, Config};

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::default();
    let _app = setup(config).await?;
    Ok(())
}
