//! Packs the app registrations into the build.
//!
//! The values come from the environment, or from the `.env` file at the
//! workspace root, at compile time. That keeps them out of the published
//! source while every binary built by a maintainer carries them, so users
//! are never asked for a client ID.

use std::path::Path;

const VARIABLES: [&str; 3] = [
    "SKYDOCK_ONEDRIVE_CLIENT_ID",
    "SKYDOCK_GDRIVE_CLIENT_ID",
    "SKYDOCK_GDRIVE_CLIENT_SECRET",
];

fn main() {
    let dotenv_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.env");
    println!("cargo:rerun-if-changed={}", dotenv_path.display());
    let dotenv = std::fs::read_to_string(&dotenv_path).unwrap_or_default();

    for name in VARIABLES {
        println!("cargo:rerun-if-env-changed={name}");
        let value = std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| from_dotenv(&dotenv, name))
            .unwrap_or_default();
        println!("cargo:rustc-env=SKYDOCK_BUILTIN_{name}={}", value.trim());
    }
}

fn from_dotenv(text: &str, name: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.strip_prefix("export ").unwrap_or(line).split_once('='))
        .find(|(key, _)| key.trim() == name)
        .map(|(_, value)| value.trim().trim_matches(['"', '\'']).to_owned())
        .filter(|value| !value.is_empty())
}
