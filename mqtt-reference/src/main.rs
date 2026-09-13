//! `mica-mqtt-reference` command-line entry point.

#![forbid(unsafe_code)]

use clap::Parser;

/// A reference-only Item1 application for the packaged MQTT bridge proof.
#[derive(Debug, Parser)]
#[command(name = "mica-mqtt-reference", version)]
struct Args {}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    mica_mqtt_reference::serve_system().await
}
