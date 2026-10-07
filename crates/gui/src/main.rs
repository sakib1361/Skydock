//! Desktop window and tray icon. Holds no logic of its own: every action
//! goes through `skydock-service`, on a background runtime so the window
//! never blocks on the network.

mod glibc_compat;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use skydock_core::ProviderKind;
use skydock_service::{CacheUsage, Mount, ProviderStatus, Service};
use slint::{CloseRequestResponse, ComponentHandle, Image, Model, ModelRc, VecModel, Weak};

slint::include_modules!();

#[derive(Clone, Copy)]
enum Action {
    Refresh,
    SignIn,
    SignOut,
    Sync,
}

/// What a card shows, as plain data that can cross threads.
struct CardState {
    configured: bool,
    signed_in: bool,
    headline: String,
    detail: String,
    storage: String,
    storage_fraction: f32,
    /// A problem that does not stop the card from showing its state.
    warning: String,
}

impl CardState {
    fn apply(self, row: &mut ProviderRow) {
        row.configured = self.configured;
        row.signed_in = self.signed_in;
        row.headline = self.headline.into();
        row.detail = self.detail.into();
        row.storage = self.storage.into();
        row.storage_fraction = self.storage_fraction;
        row.error = self.warning.into();
    }
}

#[derive(Clone)]
struct App {
    ui: Weak<MainWindow>,
    runtime: tokio::runtime::Handle,
    /// Live on-demand folders. Dropping an entry unmounts it.
    mounts: Arc<Mutex<HashMap<ProviderKind, Mount>>>,
    /// Providers with an action in progress, so a second one is not started
    /// on top (a timed sync while the user is signing out, say).
    busy: Arc<Mutex<HashSet<ProviderKind>>>,
    /// When each drive last answered a check, for display.
    last_checked: Arc<Mutex<HashMap<ProviderKind, Instant>>>,
    /// When a check was last started, successful or not, for scheduling: a
    /// drive that is unreachable is retried at the normal pace, not at once.
    last_attempt: Arc<Mutex<HashMap<ProviderKind, Instant>>>,
}

/// How often the background task wakes to refresh figures and see whether
/// a check is due.
const TICK: Duration = Duration::from_secs(15);

/// Choices offered in Settings, in minutes; 0 is "only when I ask". Must
/// match `check-choices` in main.slint.
const CHECK_INTERVALS_MINUTES: [u32; 6] = [1, 5, 15, 30, 60, 0];

impl App {
    fn update_row(
        &self,
        kind: ProviderKind,
        change: impl FnOnce(&mut ProviderRow) + Send + 'static,
    ) {
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            let model = ui.get_providers();
            let index = row_index(kind);
            if let Some(mut row) = model.row_data(index) {
                change(&mut row);
                model.set_row_data(index, row);
            }
        });
    }

    fn set_busy(&self, kind: ProviderKind, text: impl Into<String>) {
        let text = text.into();
        self.update_row(kind, move |row| {
            row.busy = true;
            row.busy_text = text.into();
            row.error = "".into();
        });
    }

    fn set_message(&self, message: String) {
        let _ = self
            .ui
            .upgrade_in_event_loop(move |ui| ui.set_message(message.into()));
    }

    fn start(&self, kind: ProviderKind, action: Action) {
        if !self.busy.lock().unwrap().insert(kind) {
            return;
        }
        self.set_busy(
            kind,
            match action {
                Action::Refresh => "Checking…",
                Action::SignIn => "Finish signing in in your browser…",
                Action::SignOut => "Signing out…",
                Action::Sync => "Checking for changes…",
            },
        );
        let signing_in = matches!(action, Action::SignIn);
        if signing_in {
            self.update_row(kind, |row| row.headline = "Signing in…".into());
        }
        let app = self.clone();
        self.runtime.spawn(async move {
            let result = app.perform(kind, action).await;
            app.busy.lock().unwrap().remove(&kind);
            let on_device = app.on_device_text(kind);
            app.update_row(kind, move |row| {
                row.busy = false;
                row.on_device = on_device.into();
                match result {
                    Ok(state) => state.apply(row),
                    Err(error) => {
                        // A failed or abandoned sign-in leaves the card as
                        // it was before the attempt.
                        if signing_in && !row.signed_in {
                            row.headline = "Not signed in".into();
                        }
                        row.error = error.to_string().into();
                    }
                }
            });
        });
    }

    /// Fetch remote changes, showing a running count on the card.
    async fn pull(
        &self,
        service: &Service,
        kind: ProviderKind,
        label: &'static str,
    ) -> skydock_service::Result<()> {
        self.set_busy(kind, format!("{label}…"));
        self.last_attempt
            .lock()
            .unwrap()
            .insert(kind, Instant::now());
        let app = self.clone();
        service
            .pull(kind, false, &move |entries| {
                if entries > 0 {
                    app.set_busy(kind, format!("{label}… {entries} items"));
                }
            })
            .await?;
        self.last_checked
            .lock()
            .unwrap()
            .insert(kind, Instant::now());
        self.update_row(kind, |row| {
            row.last_checked = last_checked_text(Duration::ZERO).into()
        });
        Ok(())
    }

    async fn perform(
        &self,
        kind: ProviderKind,
        action: Action,
    ) -> skydock_service::Result<CardState> {
        let service = Service::load()?;
        match action {
            Action::Refresh => {}
            Action::SignIn => {
                let account = service.sign_in(kind, &|_| {}).await?;
                // Show the account straight away; reading a large drive's
                // file list for the first time can take minutes.
                let signed_in = card_state(ProviderStatus::SignedIn {
                    account,
                    totals: None,
                });
                self.update_row(kind, move |row| signed_in.apply(row));
                self.first_read(&service, kind).await?;
            }
            Action::SignOut => {
                self.mounts.lock().unwrap().remove(&kind);
                service.sign_out(kind).await?;
            }
            Action::Sync => self.pull(&service, kind, "Checking for changes").await?,
        }
        let mut status = service.status(kind).await?;
        let never_finished = matches!(&status, ProviderStatus::SignedIn { totals: None, .. });
        if never_finished && matches!(action, Action::Refresh) {
            // An earlier first read was cut short; pick it up again.
            self.first_read(&service, kind).await?;
            status = service.status(kind).await?;
        }
        let ready = matches!(
            &status,
            ProviderStatus::SignedIn {
                totals: Some(_),
                ..
            }
        );
        let mut state = card_state(status);
        if ready && let Err(error) = self.ensure_mounted(&service, kind).await {
            state.warning = format!("Files are not available in the folder: {error}");
        }
        Ok(state)
    }

    /// Read a drive for the first time: its top folder at once, so it can
    /// be opened and browsed, then everything else behind it.
    async fn first_read(
        &self,
        service: &Service,
        kind: ProviderKind,
    ) -> skydock_service::Result<()> {
        self.set_busy(kind, "Reading your files…");
        // If this fails the full read below still runs, and reports why.
        if let Ok(true) = service.pull_top_level(kind).await {
            let _ = self.ensure_mounted(service, kind).await;
        }
        self.pull(service, kind, "Folder is ready; still reading your files")
            .await
    }

    /// Present the drive in its folder as files on demand.
    async fn ensure_mounted(
        &self,
        service: &Service,
        kind: ProviderKind,
    ) -> skydock_service::Result<()> {
        if self.mounts.lock().unwrap().contains_key(&kind) {
            return Ok(());
        }
        let mount = service.mount(kind, self.runtime.clone()).await?;
        self.mounts.lock().unwrap().insert(kind, mount);
        Ok(())
    }

    /// What the card says about downloaded content and changes waiting to
    /// be uploaded; empty if unknown.
    fn on_device_text(&self, kind: ProviderKind) -> String {
        Service::load()
            .and_then(|service| {
                Ok(on_device_text(
                    service.on_device(kind)?,
                    service.waiting_uploads(kind)?,
                ))
            })
            .unwrap_or_default()
    }

    /// While the application runs: keep the on-device figures current, and
    /// check mounted drives for remote changes as often as Settings says.
    fn run_in_background(&self) {
        let app = self.clone();
        self.runtime.spawn(async move {
            let mut ticks = tokio::time::interval(TICK);
            loop {
                ticks.tick().await;
                let interval = Service::load()
                    .map(|service| service.settings().check_interval_minutes())
                    .unwrap_or(0);
                let mounted: Vec<_> = app.mounts.lock().unwrap().keys().copied().collect();
                for kind in mounted {
                    let on_device = app.on_device_text(kind);
                    app.update_row(kind, move |row| row.on_device = on_device.into());

                    let last = app.last_attempt.lock().unwrap().get(&kind).copied();
                    let due = interval > 0
                        && last.is_none_or(|at| {
                            at.elapsed() >= Duration::from_secs(u64::from(interval) * 60)
                        });
                    if due {
                        app.start(kind, Action::Sync);
                    }
                }
            }
        });
    }

    fn free_up_space(&self, kind: ProviderKind) {
        let app = self.clone();
        self.runtime.spawn(async move {
            match Service::load().and_then(|service| service.free_up_space(kind)) {
                Ok(_) => {
                    let on_device = app.on_device_text(kind);
                    app.update_row(kind, move |row| row.on_device = on_device.into());
                    app.set_message(String::new());
                }
                Err(error) => app.set_message(error.to_string()),
            }
        });
    }

    fn choose_folder(&self) {
        let app = self.clone();
        self.runtime.spawn(async move {
            let Some(folder) = rfd::AsyncFileDialog::new()
                .set_title("Choose the sync folder")
                .pick_folder()
                .await
            else {
                return;
            };
            let saved = Service::load().and_then(|mut service| {
                service
                    .settings_mut()
                    .set_sync_root(folder.path().to_owned())?;
                Ok(service)
            });
            match saved {
                Ok(service) => {
                    app.set_message(String::new());
                    let _ = app
                        .ui
                        .upgrade_in_event_loop(move |ui| show_folders(&ui, &service));
                    // Move the on-demand folders to the new location.
                    app.mounts.lock().unwrap().clear();
                    for kind in ProviderKind::ALL {
                        app.start(kind, Action::Refresh);
                    }
                }
                Err(error) => app.set_message(error.to_string()),
            }
        });
    }
}

fn row_index(kind: ProviderKind) -> usize {
    ProviderKind::ALL
        .iter()
        .position(|candidate| *candidate == kind)
        .expect("every kind is in ALL")
}

fn card_state(status: ProviderStatus) -> CardState {
    let mut state = CardState {
        configured: true,
        signed_in: false,
        headline: String::new(),
        detail: String::new(),
        storage: String::new(),
        storage_fraction: -1.0,
        warning: String::new(),
    };
    match status {
        ProviderStatus::NotConfigured => {
            state.configured = false;
            state.headline = "Not set up".to_owned();
            state.detail = "This build has no app registration for this provider. Enter one to \
                            continue; it is saved in the settings file."
                .to_owned();
        }
        ProviderStatus::SignedOut => state.headline = "Not signed in".to_owned(),
        ProviderStatus::SignedIn { account, totals } => {
            state.signed_in = true;
            let who = account.email.or(account.display_name);
            state.headline = format!("Signed in as {}", who.as_deref().unwrap_or("unknown"));
            state.detail = match totals {
                Some(t) => format!(
                    "{} folders, {} files, {}",
                    t.folders,
                    t.files,
                    human_size(t.bytes)
                ),
                None => String::new(),
            };
            if let Some(used) = account.quota_used {
                match account.quota_total.filter(|total| *total > 0) {
                    Some(total) => {
                        state.storage =
                            format!("{} of {} used", human_size(used), human_size(total));
                        state.storage_fraction = (used as f64 / total as f64).min(1.0) as f32;
                    }
                    None => state.storage = format!("{} used", human_size(used)),
                }
            }
        }
    }
    state
}

fn on_device_text(usage: CacheUsage, waiting_uploads: usize) -> String {
    let downloaded = match usage.files {
        0 => "Nothing downloaded to this device".to_owned(),
        1 => format!("1 file on this device · {}", human_size(usage.bytes)),
        files => format!("{files} files on this device · {}", human_size(usage.bytes)),
    };
    match waiting_uploads {
        0 => downloaded,
        1 => format!("{downloaded} · 1 file waiting to upload"),
        files => format!("{downloaded} · {files} files waiting to upload"),
    }
}

fn last_checked_text(ago: Duration) -> String {
    match ago.as_secs() / 60 {
        0 => "Last checked just now".to_owned(),
        minutes @ 1..60 => format!("Last checked {minutes} min ago"),
        minutes => format!("Last checked {} h ago", minutes / 60),
    }
}

/// The tray tooltip: one line per drive.
fn tray_status(rows: &[ProviderRow]) -> String {
    let mut lines = vec!["Skydock".to_owned()];
    for row in rows {
        let state = if row.busy {
            row.busy_text.to_string()
        } else if !row.signed_in {
            "not signed in".to_owned()
        } else {
            [row.on_device.as_str(), row.last_checked.as_str()]
                .into_iter()
                .filter(|part| !part.is_empty())
                .map(lower_first)
                .collect::<Vec<_>>()
                .join(", ")
        };
        lines.push(format!("{}: {state}", row.name));
    }
    lines.join("\n")
}

/// A sentence as it reads mid-line. Only the first letter changes, so
/// units such as MiB keep their capitals.
fn lower_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
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

/// The provider's icon from the installed icon theme, so no third-party
/// artwork ships with the application; a plain cloud if the theme has none.
fn provider_icon(kind: ProviderKind) -> Image {
    theme_icon(match kind {
        ProviderKind::OneDrive => "folder-onedrive",
        ProviderKind::GoogleDrive => "folder-gdrive",
    })
    .unwrap_or_else(|| {
        Image::load_from_svg_data(include_bytes!("../ui/cloud.svg")).unwrap_or_default()
    })
}

fn theme_icon(name: &str) -> Option<Image> {
    let data_dirs =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".to_owned());
    for data_dir in data_dirs.split(':') {
        for theme in ["breeze", "breeze-dark", "Papirus", "hicolor"] {
            for size in ["64", "48", "32"] {
                let path = format!("{data_dir}/icons/{theme}/places/{size}/{name}.svg");
                if Path::new(&path).is_file()
                    && let Ok(image) = Image::load_from_path(Path::new(&path))
                {
                    return Some(image);
                }
            }
        }
    }
    None
}

fn show_folders(ui: &MainWindow, service: &Service) {
    ui.set_sync_root(service.settings().sync_root().display().to_string().into());
    let model = ui.get_providers();
    for kind in ProviderKind::ALL {
        let index = row_index(kind);
        if let Some(mut row) = model.row_data(index) {
            row.folder = service.provider_folder(kind).display().to_string().into();
            model.set_row_data(index, row);
        }
    }
}

fn provider_from_id(id: &str) -> Option<ProviderKind> {
    id.parse().ok()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let service = Service::load()?;
    let ui = MainWindow::new()?;
    let app = App {
        ui: ui.as_weak(),
        runtime: runtime.handle().clone(),
        mounts: Arc::default(),
        busy: Arc::default(),
        last_checked: Arc::default(),
        last_attempt: Arc::default(),
    };

    let rows: Vec<ProviderRow> = ProviderKind::ALL
        .into_iter()
        .map(|kind| ProviderRow {
            id: kind.id().into(),
            name: kind.display_name().into(),
            icon: provider_icon(kind),
            needs_secret: kind == ProviderKind::GoogleDrive,
            // Known without asking anyone, so the credential fields never
            // flash up while the first status check is still running.
            configured: service.is_configured(kind),
            storage_fraction: -1.0,
            ..Default::default()
        })
        .collect();
    ui.set_providers(ModelRc::new(VecModel::from(rows)));
    show_folders(&ui, &service);
    ui.set_version(env!("CARGO_PKG_VERSION").into());
    let minutes = service.settings().check_interval_minutes();
    ui.set_check_interval_index(
        CHECK_INTERVALS_MINUTES
            .iter()
            .position(|choice| *choice == minutes)
            .unwrap_or(1) as i32,
    );

    let on = |action: Action| {
        let app = app.clone();
        move |id: slint::SharedString| {
            if let Some(kind) = provider_from_id(&id) {
                app.start(kind, action);
            }
        }
    };
    ui.on_sign_in(on(Action::SignIn));
    ui.on_sign_out(on(Action::SignOut));
    ui.on_sync_now(on(Action::Sync));
    ui.on_open_folder(|id| {
        let folder = provider_from_id(&id)
            .zip(Service::load().ok())
            .map(|(kind, service)| service.provider_folder(kind));
        if let Some(folder) = folder {
            let _ = std::process::Command::new("xdg-open").arg(folder).spawn();
        }
    });
    ui.on_save_credentials({
        let app = app.clone();
        move |id, client_id, client_secret| {
            let Some(kind) = provider_from_id(&id) else {
                return;
            };
            let (client_id, client_secret) =
                (client_id.trim().to_owned(), client_secret.trim().to_owned());
            let saved = Service::load().and_then(|mut service| {
                let settings = service.settings_mut();
                match kind {
                    ProviderKind::OneDrive => settings.set_onedrive_client_id(client_id),
                    ProviderKind::GoogleDrive => {
                        settings.set_gdrive_credentials(client_id, client_secret)
                    }
                }
            });
            match saved {
                Ok(()) => app.start(kind, Action::Refresh),
                Err(error) => app.set_message(error.to_string()),
            }
        }
    });
    ui.on_check_interval_changed({
        let app = app.clone();
        move |index| {
            let Some(minutes) = CHECK_INTERVALS_MINUTES.get(index as usize) else {
                return;
            };
            let saved = Service::load().and_then(|mut service| {
                service.settings_mut().set_check_interval_minutes(*minutes)
            });
            app.set_message(saved.err().map(|e| e.to_string()).unwrap_or_default());
        }
    });
    ui.on_free_up_space({
        let app = app.clone();
        move |id| {
            if let Some(kind) = provider_from_id(&id) {
                app.free_up_space(kind);
            }
        }
    });
    ui.on_choose_folder({
        let app = app.clone();
        move || app.choose_folder()
    });

    for kind in ProviderKind::ALL {
        app.start(kind, Action::Refresh);
    }

    // With a tray icon, closing the window only hides it. Without one there
    // would be no way to get it back, so closing quits.
    let tray = TrayIcon::new().and_then(|tray| tray.show().map(|()| tray));
    let has_tray = tray.is_ok();
    match &tray {
        Ok(tray) => {
            let ui_weak = ui.as_weak();
            tray.on_open(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    let _ = ui.show();
                }
            });
            tray.on_quit(|| {
                let _ = slint::quit_event_loop();
            });
        }
        Err(error) => eprintln!("skydock: no tray icon: {error}"),
    }
    ui.window().on_close_requested(move || {
        if !has_tray {
            let _ = slint::quit_event_loop();
        }
        CloseRequestResponse::HideWindow
    });

    app.run_in_background();
    // On the UI thread: age the "last checked" labels and keep the tray
    // tooltip in step with the cards.
    let labels = slint::Timer::default();
    labels.start(slint::TimerMode::Repeated, Duration::from_secs(5), {
        let ui_weak = ui.as_weak();
        let tray_weak = tray.as_ref().ok().map(|tray| tray.as_weak());
        let last_checked = Arc::clone(&app.last_checked);
        move || {
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };
            let model = ui.get_providers();
            for kind in ProviderKind::ALL {
                let checked = last_checked.lock().unwrap().get(&kind).copied();
                let index = row_index(kind);
                if let (Some(checked), Some(mut row)) = (checked, model.row_data(index)) {
                    let text = last_checked_text(checked.elapsed());
                    if row.last_checked != text.as_str() {
                        row.last_checked = text.into();
                        model.set_row_data(index, row);
                    }
                }
            }
            if let Some(tray) = tray_weak.as_ref().and_then(Weak::upgrade) {
                let rows: Vec<_> = model.iter().collect();
                tray.set_status(tray_status(&rows).into());
            }
        }
    });
    // Leave through the normal exit path on logout or Ctrl+C, so the
    // on-demand folders are unmounted rather than left dead.
    runtime.spawn(async {
        use tokio::signal::unix::{SignalKind, signal};
        if let (Ok(mut terminate), Ok(mut interrupt)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) {
            tokio::select! {
                _ = terminate.recv() => {}
                _ = interrupt.recv() => {}
            }
            let _ = slint::quit_event_loop();
        }
    });
    ui.show()?;
    slint::run_event_loop_until_quit()?;
    // Unmount before the runtime that serves the mounts goes away.
    app.mounts.lock().unwrap().clear();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_checked_reads_naturally_at_each_scale() {
        let text = |seconds| last_checked_text(Duration::from_secs(seconds));
        assert_eq!(text(0), "Last checked just now");
        assert_eq!(text(59), "Last checked just now");
        assert_eq!(text(60), "Last checked 1 min ago");
        assert_eq!(text(59 * 60), "Last checked 59 min ago");
        assert_eq!(text(3 * 3600 + 10), "Last checked 3 h ago");
    }

    #[test]
    fn on_device_line_handles_none_one_and_many() {
        let text = |files, bytes| on_device_text(CacheUsage { files, bytes }, 0);
        assert_eq!(text(0, 0), "Nothing downloaded to this device");
        assert_eq!(text(1, 512), "1 file on this device · 512 B");
        assert_eq!(
            text(38, 3 * 1024 * 1024),
            "38 files on this device · 3.0 MiB"
        );
    }

    #[test]
    fn on_device_line_mentions_files_waiting_to_upload() {
        let text = |waiting| {
            on_device_text(
                CacheUsage {
                    files: 1,
                    bytes: 512,
                },
                waiting,
            )
        };
        assert_eq!(
            text(1),
            "1 file on this device · 512 B · 1 file waiting to upload"
        );
        assert_eq!(
            text(4),
            "1 file on this device · 512 B · 4 files waiting to upload"
        );
    }

    #[test]
    fn tray_status_has_a_line_per_drive() {
        let row = |name: &str, signed_in, busy| ProviderRow {
            name: name.into(),
            signed_in,
            busy,
            busy_text: "Checking for changes…".into(),
            on_device: "38 files on this device · 3.0 MiB".into(),
            last_checked: "Last checked 2 min ago".into(),
            ..Default::default()
        };
        assert_eq!(
            tray_status(&[
                row("OneDrive", true, false),
                row("Google Drive", false, false),
                row("Other", true, true),
            ]),
            "Skydock\n\
             OneDrive: 38 files on this device · 3.0 MiB, last checked 2 min ago\n\
             Google Drive: not signed in\n\
             Other: Checking for changes…"
        );
    }
}
