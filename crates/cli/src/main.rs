mod config;

use anyhow::Result;
use clap::{Parser, Subcommand};
use odl_graph::delta::{DeltaLink, latest_per_item};
use odl_graph::model::normalize_drive_id;
use odl_graph::{Authenticator, GraphClient};

#[derive(Parser)]
#[command(name = "odl", version, about = "OneDrive client for Linux")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Sign in through the browser and store the session in the keyring
    Login,
    /// Remove the stored session
    Logout,
    /// Show the signed-in account's drive and quota
    Drive,
    /// Enumerate the whole drive through delta and print a summary
    Delta {
        /// Print every item as a JSON line instead of the summary
        #[arg(long)]
        json: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = GraphClient::new(Authenticator::new(config::client_id()?)?);

    match cli.command {
        Command::Login => {
            client
                .auth()
                .sign_in(|url| {
                    eprintln!(
                        "Opening your browser to sign in. If nothing opens, visit:\n\n{url}\n"
                    )
                })
                .await?;
            let drive = client.my_drive().await?;
            println!("Signed in. Drive {}", normalize_drive_id(&drive.id));
        }
        Command::Logout => {
            client.auth().sign_out().await?;
            println!("Signed out.");
        }
        Command::Drive => {
            let drive = client.my_drive().await?;
            println!("id:    {}", normalize_drive_id(&drive.id));
            println!(
                "type:  {}",
                drive.drive_type.as_deref().unwrap_or("unknown")
            );
            if let Some(quota) = drive.quota {
                println!("used:  {}", quota.used.map_or("unknown".into(), human_size));
                println!(
                    "total: {}",
                    quota.total.map_or("unknown".into(), human_size)
                );
            }
        }
        Command::Delta { json } => delta(&client, json).await?,
    }
    Ok(())
}

async fn delta(client: &GraphClient, json: bool) -> Result<()> {
    let mut url = GraphClient::delta_start_url();
    let mut items = Vec::new();
    let mut pages = 0;
    loop {
        let page = client.delta_page(&url).await?;
        pages += 1;
        items.extend(page.items);
        eprint!("\rpage {pages}, {} entries", items.len());
        match page.link {
            DeltaLink::Next(next) => url = next,
            DeltaLink::Delta(_) => break,
        }
    }
    eprintln!();

    let items = latest_per_item(items);
    if json {
        for item in &items {
            println!("{}", serde_json::to_string(item)?);
        }
        return Ok(());
    }

    let live = || items.iter().filter(|item| !item.is_deleted());
    let files = live().filter(|item| item.is_file()).count();
    let folders = live().filter(|item| item.is_folder()).count();
    let bytes: u64 = live()
        .filter(|item| item.is_file())
        .filter_map(|item| item.size)
        .sum();
    let without_hash = live()
        .filter(|item| item.is_file() && item.quick_xor_hash().is_none())
        .count();

    println!("folders:            {folders}");
    println!("files:              {files} ({})", human_size(bytes));
    println!("files without hash: {without_hash}");
    println!("deleted entries:    {}", items.len() - live().count());
    println!(
        "shared shortcuts:   {}",
        live().filter(|i| i.remote_item.is_some()).count()
    );
    Ok(())
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
