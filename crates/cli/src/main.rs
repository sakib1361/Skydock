use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use skydock_core::ProviderKind;
use skydock_service::{ProviderStatus, Service};

#[derive(Parser)]
#[command(name = "skydock", version, about = "Cloud drive client for Linux")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List providers with their sign-in state and local folder
    Providers,
    /// Sign in through the browser and store the session in the keyring
    Login { provider: ProviderKind },
    /// Remove the stored session
    Logout { provider: ProviderKind },
    /// Fetch remote changes into the local state database
    Pull {
        provider: ProviderKind,
        /// Read the whole drive again instead of only the changes
        #[arg(long)]
        full: bool,
    },
    /// Show the drive as a folder of on-demand files until interrupted
    Mount { provider: ProviderKind },
    /// List a folder from the fetched file list
    Ls {
        provider: ProviderKind,
        #[arg(default_value = "/")]
        path: String,
    },
    /// Download one file and check it against the provider's hash
    Get {
        provider: ProviderKind,
        /// Path within the drive, for example /Documents/report.pdf
        path: String,
        /// Where to save it; defaults to its place in the provider's folder
        #[arg(long)]
        to: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let service = Service::load()?;

    match cli.command {
        Command::Providers => {
            for kind in ProviderKind::ALL {
                println!("{} ({})", kind.display_name(), kind.id());
                println!("  folder: {}", service.provider_folder(kind).display());
                match service.status(kind).await {
                    Ok(ProviderStatus::NotConfigured) => println!(
                        "  not configured; add its client ID to {}",
                        service.settings().path().display()
                    ),
                    Ok(ProviderStatus::SignedOut) => println!("  signed out"),
                    Ok(ProviderStatus::SignedIn { account, totals }) => {
                        let who = account.email.or(account.display_name);
                        println!("  signed in as {}", who.as_deref().unwrap_or("unknown"));
                        if let Some(used) = account.quota_used {
                            let total = account.quota_total.map_or("unlimited".into(), human_size);
                            println!("  storage: {} of {total}", human_size(used));
                        }
                        match totals {
                            Some(t) => println!(
                                "  known: {} folders, {} files, {}",
                                t.folders,
                                t.files,
                                human_size(t.bytes)
                            ),
                            None => {
                                println!("  nothing fetched yet; run `skydock pull {}`", kind.id())
                            }
                        }
                    }
                    Err(e) => println!("  error: {e}"),
                }
            }
        }
        Command::Login { provider } => {
            let account = service
                .sign_in(provider, &|url| {
                    eprintln!(
                        "Opening your browser to sign in. If nothing opens, visit:\n\n{url}\n"
                    )
                })
                .await?;
            let who = account.email.or(account.display_name);
            println!(
                "Signed in to {provider} as {}. Folder: {}",
                who.as_deref().unwrap_or("unknown"),
                service.provider_folder(provider).display()
            );
        }
        Command::Logout { provider } => {
            service.sign_out(provider).await?;
            println!("Signed out of {provider}.");
        }
        Command::Pull { provider, full } => {
            let report = service
                .pull(provider, full, &|entries| {
                    eprint!("\r{entries} entries received")
                })
                .await?;
            eprintln!();
            println!(
                "{}: {} added or updated, {} removed",
                if report.full {
                    "Full enumeration"
                } else {
                    "Changes"
                },
                report.applied.upserted,
                report.applied.deleted
            );
            if report.applied.folders_kept > 0 {
                println!(
                    "{} deleted folders kept because they still contain items",
                    report.applied.folders_kept
                );
            }
            println!(
                "{} folders, {} files, {}",
                report.totals.folders,
                report.totals.files,
                human_size(report.totals.bytes)
            );
        }
        Command::Mount { provider } => {
            let mount = service
                .mount(provider, tokio::runtime::Handle::current())
                .await?;
            println!(
                "{provider} is mounted at {}. Press Ctrl+C to unmount.",
                mount.mountpoint().display()
            );
            tokio::signal::ctrl_c().await?;
            drop(mount);
            println!("Unmounted.");
        }
        Command::Ls { provider, path } => {
            for item in service.list(provider, &path).await? {
                let size = match (item.is_folder, item.size) {
                    (true, _) => "<folder>".to_owned(),
                    (false, Some(size)) => human_size(size),
                    (false, None) => "-".to_owned(),
                };
                println!("{size:>10}  {}", item.name);
            }
        }
        Command::Get { provider, path, to } => {
            let report = service.download(provider, &path, to.as_deref()).await?;
            println!(
                "Saved {} ({}, {})",
                report.dest.display(),
                human_size(report.bytes),
                if report.hash_checked {
                    "hash verified"
                } else {
                    "size verified; the provider gives no hash"
                }
            );
        }
    }
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
