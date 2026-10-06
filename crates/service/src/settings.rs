use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, Result, builtin};

/// Contents of `~/.config/skydock/config.toml`.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
struct SettingsFile {
    /// The folder the user chose. Each provider gets a subfolder in it.
    sync_root: Option<PathBuf>,
    /// Minutes between automatic checks for remote changes; 0 turns them
    /// off. Absent means the default.
    check_interval_minutes: Option<u32>,
    onedrive: Option<OneDriveSettings>,
    gdrive: Option<GoogleDriveSettings>,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
struct OneDriveSettings {
    client_id: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
struct GoogleDriveSettings {
    client_id: Option<String>,
    client_secret: Option<String>,
}

/// A check costs one small request per drive, so this could be shorter;
/// five minutes keeps a laptop's radio and the provider's rate limits out
/// of the picture until push notifications replace polling.
pub const DEFAULT_CHECK_INTERVAL_MINUTES: u32 = 5;

#[derive(Debug, Clone)]
pub struct Settings {
    path: PathBuf,
    home: PathBuf,
    file: SettingsFile,
    /// Variables from a `.env` file in the working directory, a convenience
    /// for running from a source checkout.
    dotenv: HashMap<String, String>,
    builtin: Builtin,
}

/// The registrations packed into the build; a field so tests can run
/// without them.
#[derive(Debug, Clone, Copy, Default)]
struct Builtin {
    onedrive_client_id: &'static str,
    gdrive_client_id: &'static str,
    gdrive_client_secret: &'static str,
}

impl Settings {
    pub fn load() -> Result<Self> {
        let path = dirs::config_dir()
            .ok_or(Error::NoUserDir("config"))?
            .join("skydock/config.toml");
        let home = dirs::home_dir().ok_or(Error::NoUserDir("home"))?;
        let file = match std::fs::read_to_string(&path) {
            Ok(text) => parse(&path, &text)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => SettingsFile::default(),
            Err(source) => {
                return Err(Error::Io {
                    context: format!("cannot read {}", path.display()),
                    source,
                });
            }
        };
        let dotenv = std::fs::read_to_string(".env")
            .map(|text| parse_dotenv(&text))
            .unwrap_or_default();
        Ok(Self {
            path,
            home,
            file,
            dotenv,
            builtin: Builtin {
                onedrive_client_id: builtin::ONEDRIVE_CLIENT_ID,
                gdrive_client_id: builtin::GDRIVE_CLIENT_ID,
                gdrive_client_secret: builtin::GDRIVE_CLIENT_SECRET,
            },
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The chosen sync folder; the home directory until the user picks one,
    /// which gives `~/OneDrive` and `~/Google Drive`.
    pub fn sync_root(&self) -> PathBuf {
        self.file
            .sync_root
            .clone()
            .unwrap_or_else(|| self.home.clone())
    }

    /// Minutes between automatic checks for remote changes; 0 means only
    /// when the user asks.
    pub fn check_interval_minutes(&self) -> u32 {
        self.file
            .check_interval_minutes
            .unwrap_or(DEFAULT_CHECK_INTERVAL_MINUTES)
    }

    pub fn set_check_interval_minutes(&mut self, minutes: u32) -> Result<()> {
        self.file.check_interval_minutes = Some(minutes);
        self.save()
    }

    pub fn set_sync_root(&mut self, folder: PathBuf) -> Result<()> {
        self.file.sync_root = Some(folder);
        self.save()
    }

    /// Environment first, then a `.env` in the working directory, then the
    /// settings file, then the registration built into this build. Users
    /// normally rely on the last.
    pub fn onedrive_client_id(&self) -> Option<String> {
        setting(
            "SKYDOCK_ONEDRIVE_CLIENT_ID",
            &self.dotenv,
            self.file
                .onedrive
                .as_ref()
                .and_then(|s| s.client_id.as_deref()),
            self.builtin.onedrive_client_id,
        )
    }

    pub fn gdrive_credentials(&self) -> Option<(String, String)> {
        let file = self.file.gdrive.as_ref();
        Some((
            setting(
                "SKYDOCK_GDRIVE_CLIENT_ID",
                &self.dotenv,
                file.and_then(|s| s.client_id.as_deref()),
                self.builtin.gdrive_client_id,
            )?,
            setting(
                "SKYDOCK_GDRIVE_CLIENT_SECRET",
                &self.dotenv,
                file.and_then(|s| s.client_secret.as_deref()),
                self.builtin.gdrive_client_secret,
            )?,
        ))
    }

    pub fn set_onedrive_client_id(&mut self, client_id: String) -> Result<()> {
        self.file.onedrive.get_or_insert_default().client_id = Some(client_id);
        self.save()
    }

    pub fn set_gdrive_credentials(
        &mut self,
        client_id: String,
        client_secret: String,
    ) -> Result<()> {
        let gdrive = self.file.gdrive.get_or_insert_default();
        gdrive.client_id = Some(client_id);
        gdrive.client_secret = Some(client_secret);
        self.save()
    }

    fn save(&self) -> Result<()> {
        let io = |context: String| move |source| Error::Io { context, source };
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(io(format!("cannot create {}", dir.display())))?;
        }
        let text = toml::to_string_pretty(&self.file).expect("settings serialize to TOML");
        std::fs::write(&self.path, text)
            .map_err(io(format!("cannot write {}", self.path.display())))
    }
}

fn parse(path: &Path, text: &str) -> Result<SettingsFile> {
    toml::from_str(text).map_err(|e| Error::Settings {
        path: path.to_owned(),
        message: e.to_string(),
    })
}

/// `KEY=VALUE` lines; blank lines and `#` comments are skipped, and one
/// pair of surrounding quotes is removed from a value.
fn parse_dotenv(text: &str) -> HashMap<String, String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.strip_prefix("export ").unwrap_or(line).split_once('='))
        .map(|(key, value)| {
            let value = value.trim();
            let unquoted = ['"', '\'']
                .into_iter()
                .find_map(|q| value.strip_prefix(q)?.strip_suffix(q))
                .unwrap_or(value);
            (key.trim().to_owned(), unquoted.to_owned())
        })
        .collect()
}

fn setting(
    env: &str,
    dotenv: &HashMap<String, String>,
    from_file: Option<&str>,
    builtin: &str,
) -> Option<String> {
    let from_env = std::env::var(env).ok();
    let from_dotenv = dotenv.get(env).map(String::as_str);
    [from_env.as_deref(), from_dotenv, from_file, Some(builtin)]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|value| !value.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(text: &str) -> Settings {
        Settings {
            path: PathBuf::from("/nonexistent/config.toml"),
            home: PathBuf::from("/home/user"),
            file: parse(Path::new("test"), text).unwrap(),
            dotenv: HashMap::new(),
            builtin: Builtin::default(),
        }
    }

    #[test]
    fn sync_root_defaults_to_home() {
        assert_eq!(settings("").sync_root(), PathBuf::from("/home/user"));
        assert_eq!(
            settings("sync_root = '/data/Cloud'").sync_root(),
            PathBuf::from("/data/Cloud")
        );
    }

    #[test]
    fn provider_sections_are_independent() {
        let s = settings("[gdrive]\nclient_id = 'g'\nclient_secret = 's'\n");
        assert_eq!(
            s.gdrive_credentials(),
            Some(("g".to_owned(), "s".to_owned()))
        );
        assert_eq!(s.onedrive_client_id(), None);
    }

    #[test]
    fn a_build_with_registrations_needs_no_settings() {
        let mut s = settings("");
        assert_eq!(s.onedrive_client_id(), None);
        s.builtin = Builtin {
            onedrive_client_id: "od",
            gdrive_client_id: "gd",
            gdrive_client_secret: "gs",
        };
        assert_eq!(s.onedrive_client_id(), Some("od".to_owned()));
        assert_eq!(
            s.gdrive_credentials(),
            Some(("gd".to_owned(), "gs".to_owned()))
        );
    }

    #[test]
    fn check_interval_defaults_and_zero_means_manual() {
        assert_eq!(
            settings("").check_interval_minutes(),
            DEFAULT_CHECK_INTERVAL_MINUTES
        );
        assert_eq!(
            settings("check_interval_minutes = 0").check_interval_minutes(),
            0
        );
        assert_eq!(
            settings("check_interval_minutes = 30").check_interval_minutes(),
            30
        );
    }

    #[test]
    fn google_needs_both_id_and_secret() {
        assert_eq!(
            settings("[gdrive]\nclient_id = 'g'\n").gdrive_credentials(),
            None
        );
    }

    #[test]
    fn blank_values_count_as_unset() {
        assert_eq!(
            setting(
                "SKYDOCK_TEST_UNSET_VARIABLE",
                &HashMap::new(),
                Some("  "),
                ""
            ),
            None
        );
        assert_eq!(
            setting(
                "SKYDOCK_TEST_UNSET_VARIABLE",
                &HashMap::new(),
                Some(" abc "),
                ""
            ),
            Some("abc".to_owned())
        );
    }

    #[test]
    fn built_in_registration_is_the_fallback_and_the_file_overrides_it() {
        let unset = "SKYDOCK_TEST_UNSET_VARIABLE";
        assert_eq!(
            setting(unset, &HashMap::new(), None, "shipped"),
            Some("shipped".to_owned())
        );
        assert_eq!(
            setting(unset, &HashMap::new(), Some("mine"), "shipped"),
            Some("mine".to_owned())
        );
    }

    #[test]
    fn unknown_keys_are_tolerated_and_bad_toml_is_reported() {
        assert!(parse(Path::new("t"), "future_option = 1").is_ok());
        assert!(matches!(
            parse(Path::new("t"), "sync_root = ["),
            Err(Error::Settings { .. })
        ));
    }

    #[test]
    fn saved_settings_read_back() {
        let mut s = settings("[onedrive]\nclient_id = 'abc'\n");
        s.file.sync_root = Some(PathBuf::from("/data/Cloud"));
        let text = toml::to_string_pretty(&s.file).unwrap();
        assert_eq!(parse(Path::new("t"), &text).unwrap(), s.file);
    }

    #[test]
    fn dotenv_lines_are_parsed_and_rank_above_the_settings_file() {
        let dotenv = parse_dotenv(
            "# comment\n\nA=1\nexport B = \"two words\"\nC='x=y'\nEMPTY=\nnot a pair\n",
        );
        assert_eq!(dotenv["A"], "1");
        assert_eq!(dotenv["B"], "two words");
        assert_eq!(dotenv["C"], "x=y");
        assert_eq!(dotenv["EMPTY"], "");
        assert_eq!(dotenv.len(), 4);

        assert_eq!(
            setting("A", &dotenv, Some("file"), "shipped"),
            Some("1".to_owned())
        );
        // An empty entry does not mask the sources below it.
        assert_eq!(
            setting("EMPTY", &dotenv, Some("file"), "shipped"),
            Some("file".to_owned())
        );
    }
}
