use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "mica-ui-pack",
    about = "Build and inspect deterministic mica UI packages"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Pack {
        source: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long)]
        name: String,
        #[arg(long)]
        version: String,
        #[arg(long = "immutable-dir", default_value = "assets")]
        immutable_dir: String,
        #[arg(long = "api-version", default_value = "v1")]
        api_versions: Vec<String>,
    },
    Inspect {
        package: PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    let info = match Cli::parse().command {
        Command::Pack {
            source,
            output,
            name,
            version,
            immutable_dir,
            api_versions,
        } => {
            let manifest = mica_ui_bundle::PackageManifest {
                schema_version: 1,
                name,
                version,
                immutable_dir,
                api_versions,
            };
            mica_ui_bundle::pack_with_manifest(&source, &output, &manifest)?
        }
        Command::Inspect { package } => mica_ui_bundle::inspect(&package)?,
    };
    println!("{}", serde_json::to_string_pretty(&info)?);
    Ok(())
}
