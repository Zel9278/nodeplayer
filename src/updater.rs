//! Velopack lifecycle and background updates. Downloading never interrupts playback;
//! applying waits for the normal app shutdown so session/plugin state can be saved.

use crossbeam_channel::{Receiver, Sender};
use std::time::{Duration, Instant};
use velopack::{UpdateCheck, UpdateInfo, UpdateManager, sources};

pub const APP_ID: &str = "Zel9278.nodeplayer";
const REPOSITORY: &str = "https://github.com/Zel9278/nodeplayer";
const RELEASES: &str = "https://github.com/Zel9278/nodeplayer/releases";
const CHECK_INTERVAL: Duration = Duration::from_secs(60 * 60);

pub fn handle_hooks() {
    let first = std::env::args_os().nth(1);
    if matches!(
        first.as_deref().and_then(|s| s.to_str()),
        Some(
            "--veloapp-install"
                | "--veloapp-updated"
                | "--veloapp-obsolete"
                | "--veloapp-uninstall"
        )
    ) {
        run_bootstrap();
    }
}

pub fn bootstrap() {
    if is_managed() {
        run_bootstrap();
    }
}

fn run_bootstrap() {
    // Velopack's builder reads UTF-8 args. Keep support for Unix paths that aren't UTF-8;
    // only its lifecycle hooks need argument handling, not a normal file-open launch.
    if std::env::args_os().any(|s| s.to_str().is_none()) {
        return;
    }
    let app = velopack::VelopackApp::build().set_auto_apply_on_startup(false);
    #[cfg(target_os = "windows")]
    let app = app.set_app_user_model_id(APP_ID);
    let mut app = app;
    app.run();
}

pub fn is_managed() -> bool {
    UpdateManager::new(sources::NoneSource {}, None, None).is_ok()
}

fn manager() -> Result<UpdateManager, velopack::Error> {
    // Inherit the architecture-specific channel recorded by vpk in this package.
    UpdateManager::new(
        sources::GithubSource::new(REPOSITORY, None, false),
        None,
        None,
    )
}

enum State {
    Unmanaged,
    Checking,
    Current,
    Empty,
    Available(Box<UpdateInfo>),
    Downloading,
    Ready(Box<UpdateInfo>),
    Failed(String),
    Restarting,
}

enum Event {
    Checked(Result<UpdateCheck, String>),
    Downloaded(Result<Box<UpdateInfo>, String>),
}

pub struct Updater {
    state: State,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    ctx: egui::Context,
    enabled: bool,
    next_check: Option<Instant>,
}

impl Updater {
    pub fn new(ctx: egui::Context) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let enabled = !crate::runtime::get().is_diagnostic() && is_managed();
        let mut this = Self {
            state: State::Unmanaged,
            tx,
            rx,
            ctx,
            enabled,
            next_check: None,
        };
        if enabled {
            // Keep a previously downloaded update available after a normal quit/reopen.
            if let Ok(manager) = manager()
                && let Some(asset) = manager.get_update_pending_restart()
            {
                this.state = State::Ready(Box::new(UpdateInfo {
                    TargetFullRelease: asset,
                    ..Default::default()
                }));
            } else {
                this.check();
            }
        }
        this
    }

    pub fn has_update(&self) -> bool {
        matches!(self.state, State::Available(_) | State::Ready(_))
    }

    fn check(&mut self) {
        if !self.enabled {
            return;
        }
        self.state = State::Checking;
        self.next_check = None;
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let result = manager()
                .and_then(|m| m.check_for_updates())
                .map_err(|e| e.to_string());
            let _ = tx.send(Event::Checked(result));
            ctx.request_repaint();
        });
    }

    fn download(&mut self, update: Box<UpdateInfo>) {
        self.state = State::Downloading;
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let result = manager()
                .and_then(|m| m.download_updates(&update, None))
                .map(|_| update)
                .map_err(|e| e.to_string());
            let _ = tx.send(Event::Downloaded(result));
            ctx.request_repaint();
        });
    }

    pub fn poll(&mut self) {
        if self.poll_at(Instant::now()) {
            self.check();
        }
    }

    /// Return whether another background check is due. Keeping time explicit lets tests
    /// cover long sessions and suspend/resume without sleeping or contacting GitHub.
    fn poll_at(&mut self, now: Instant) -> bool {
        while let Ok(event) = self.rx.try_recv() {
            self.state = match event {
                Event::Checked(Ok(UpdateCheck::UpdateAvailable(update))) => {
                    State::Available(update)
                }
                Event::Checked(Ok(UpdateCheck::NoUpdateAvailable)) => State::Current,
                Event::Checked(Ok(UpdateCheck::RemoteIsEmpty)) => State::Empty,
                Event::Downloaded(Ok(update)) => State::Ready(update),
                Event::Checked(Err(error)) | Event::Downloaded(Err(error)) => State::Failed(error),
            };
            self.next_check = Some(now + CHECK_INTERVAL);
        }
        // Once a release has been found, keep its download/restart controls intact.
        // Failed requests retry on the same cadence instead of on every frame.
        if self.enabled && matches!(self.state, State::Current | State::Empty | State::Failed(_)) {
            let deadline = self.next_check.get_or_insert(now + CHECK_INTERVAL);
            if now >= *deadline {
                return true;
            }
            self.ctx
                .request_repaint_after(deadline.saturating_duration_since(now));
        }
        false
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        let mut download = None;
        let mut restart = None;
        let mut check = false;
        match &self.state {
            State::Unmanaged => {
                ui.label("Install a release package to enable automatic updates.");
                ui.hyperlink_to("Get nodeplayer", RELEASES);
            }
            State::Checking => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Checking for updates…");
                });
            }
            State::Downloading => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Downloading update…");
                });
            }
            State::Available(update) => {
                ui.label(format!(
                    "Version {} is available.",
                    update.TargetFullRelease.Version
                ));
                ui.hyperlink_to("Release notes", RELEASES);
                if ui.button("Download update").clicked() {
                    download = Some(update.clone());
                }
            }
            State::Ready(update) => {
                ui.label(format!(
                    "Version {} is ready to install.",
                    update.TargetFullRelease.Version
                ));
                if ui.button("Restart and update").clicked() {
                    restart = Some(update.clone());
                }
            }
            State::Current | State::Empty | State::Failed(_) => {
                match &self.state {
                    State::Current => {
                        ui.label("You're up to date.");
                    }
                    State::Empty => {
                        ui.label("No updates have been published yet.");
                    }
                    State::Failed(error) => {
                        ui.label("Couldn't check or download the update.")
                            .on_hover_text(error);
                    }
                    _ => {}
                }
                check = ui.button("Check for updates").clicked();
            }
            State::Restarting => {
                ui.label("Closing nodeplayer to install the update…");
            }
        }
        if check {
            self.check();
        }
        if let Some(update) = download {
            self.download(update);
        }
        if let Some(update) = restart {
            let options = crate::runtime::get();
            let mut args = Vec::<std::ffi::OsString>::new();
            if let Some(root) = &options.data_root {
                args.extend(["--data-root".into(), root.as_os_str().to_owned()]);
            }
            if !options.experimental_plugin_isolation {
                args.push("--in-process-plugins".into());
            }
            match manager()
                .and_then(|m| m.wait_exit_then_apply_updates(&*update, false, true, args))
            {
                Ok(()) => {
                    self.state = State::Restarting;
                    // on_exit shuts down audio/workers and saves the session before the
                    // helper replaces the application and relaunches it.
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
                Err(e) => self.state = State::Failed(e.to_string()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use velopack::locator::VelopackLocatorConfig;

    #[test]
    fn periodic_checks_wait_an_hour_after_completion_and_retry_failures() {
        let now = Instant::now();
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut updater = Updater {
            state: State::Checking,
            tx,
            rx,
            ctx: egui::Context::default(),
            enabled: true,
            next_check: None,
        };
        // An in-flight request cannot be duplicated, even after a long sleep.
        assert!(!updater.poll_at(now + CHECK_INTERVAL * 2));
        updater
            .tx
            .send(Event::Checked(Ok(UpdateCheck::NoUpdateAvailable)))
            .unwrap();
        assert!(!updater.poll_at(now));
        assert!(!updater.poll_at(now + CHECK_INTERVAL - Duration::from_secs(1)));
        assert!(updater.poll_at(now + CHECK_INTERVAL));
        // Failure gets a fresh deadline, rather than immediately retrying an overdue one.
        updater
            .tx
            .send(Event::Checked(Err("offline".into())))
            .unwrap();
        let resumed = now + CHECK_INTERVAL * 3;
        assert!(!updater.poll_at(resumed));
        assert!(updater.poll_at(resumed + CHECK_INTERVAL));
        updater.enabled = false;
        assert!(!updater.poll_at(resumed + CHECK_INTERVAL * 2));
    }

    #[test]
    fn periodic_checks_preserve_available_downloading_and_pending_updates() {
        let now = Instant::now();
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut updater = Updater {
            state: State::Checking,
            tx,
            rx,
            ctx: egui::Context::default(),
            enabled: true,
            next_check: None,
        };
        for state in [
            State::Available(Default::default()),
            State::Downloading,
            State::Ready(Default::default()),
            State::Restarting,
            State::Unmanaged,
        ] {
            updater.state = state;
            updater.next_check = Some(now);
            assert!(!updater.poll_at(now + CHECK_INTERVAL));
        }
        updater
            .tx
            .send(Event::Checked(Ok(UpdateCheck::RemoteIsEmpty)))
            .unwrap();
        assert!(!updater.poll_at(now));
        assert!(updater.poll_at(now + CHECK_INTERVAL));
    }

    #[test]
    fn feed_uses_installed_channel_rejects_downgrades_and_corrupt_downloads() {
        let dir = std::env::temp_dir().join(format!("nodeplayer-update-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = dir.join("sq.version");
        std::fs::write(&manifest, format!("<package><metadata><id>{APP_ID}</id><version>1.0.0</version><mainExe>nodeplayer.exe</mainExe><channel>test-x64</channel></metadata></package>")).unwrap();
        let helper = dir.join("Update.exe");
        std::fs::write(&helper, b"not executed by this test").unwrap();
        let config = VelopackLocatorConfig {
            RootAppDir: dir.clone(),
            UpdateExePath: helper,
            PackagesDir: dir.join("packages"),
            ManifestPath: manifest,
            CurrentBinaryDir: dir.clone(),
            IsPortable: true,
        };
        let manager =
            UpdateManager::new(sources::FileSource::new(&dir), None, Some(config)).unwrap();
        let feed = dir.join("releases.test-x64.json");
        let write_feed = |version: &str| {
            std::fs::write(
                &feed,
                serde_json::json!({"Assets": [{
                    "PackageId": APP_ID, "Version": version, "Type": "Full",
                    "FileName": "Zel9278.nodeplayer-1.1.0-full.nupkg", "Size": 7,
                    "SHA256": "0".repeat(64), "SHA1": "0".repeat(40)
                }]})
                .to_string(),
            )
            .unwrap();
        };
        write_feed("0.9.0");
        assert!(matches!(
            manager.check_for_updates().unwrap(),
            UpdateCheck::NoUpdateAvailable
        ));
        write_feed("1.1.0");
        let UpdateCheck::UpdateAvailable(update) = manager.check_for_updates().unwrap() else {
            panic!("newer release on the installed channel must be found");
        };
        std::fs::write(dir.join(&update.TargetFullRelease.FileName), b"corrupt").unwrap();
        assert!(matches!(
            manager.download_updates(&update, None),
            Err(velopack::Error::ChecksumInvalid(..))
        ));
        assert!(
            !dir.join("packages")
                .join(&update.TargetFullRelease.FileName)
                .exists()
        );
        drop(manager);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
