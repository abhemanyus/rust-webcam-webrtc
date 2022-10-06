use std::env::args;

use anyhow::Result;
use tas_rust::{setup, Config};

#[tokio::main]
async fn main() -> Result<()> {
    let config = if let Some(path) = args().nth(1) {
        println!("Loading config at {}", &path);
        Config::parse(&path)?
    } else {
        println!("no config found, using default");
        Config::default()
    };
    let _ = setup(config).await?;
    Ok(())
}
