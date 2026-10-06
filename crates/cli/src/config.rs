use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const CLIENT_ID_ENV: &str = "ODL_CLIENT_ID";

#[derive(Deserialize, Default)]
struct ConfigFile {
    client_id: Option<String>,
}

pub fn config_path() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("cannot determine the user config directory")?
        .join("odl/config.toml"))
}

/// The Entra application (client) ID: environment first, then config file.
pub fn client_id() -> Result<String> {
    if let Ok(id) = std::env::var(CLIENT_ID_ENV)
        && !id.trim().is_empty()
    {
        return Ok(id.trim().to_owned());
    }

    let path = config_path()?;
    let file: ConfigFile = match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ConfigFile::default(),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };

    match file.client_id {
        Some(id) if !id.trim().is_empty() => Ok(id.trim().to_owned()),
        _ => bail!(
            "no client ID configured. Set {CLIENT_ID_ENV} or add\n    client_id = \"<application (client) ID>\"\nto {}",
            path.display()
        ),
    }
}
