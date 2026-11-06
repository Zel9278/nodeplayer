//! The normal env_logger output, also retained in a bounded buffer for the Logs dock.
//! Logging from an audio/plugin thread never waits for the UI's buffer lock.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const CAPACITY: usize = 2000;
static BUFFER: OnceLock<Mutex<Buffer>> = OnceLock::new();

#[derive(Clone)]
struct Entry {
    elapsed: Duration,
    level: log::Level,
    target: String,
    message: String,
}

impl Entry {
    fn line(&self) -> String {
        format!(
            "{:02}:{:02}.{:03} {:5} {}: {}",
            self.elapsed.as_secs() / 60,
            self.elapsed.as_secs() % 60,
            self.elapsed.subsec_millis(),
            self.level,
            self.target,
            self.message
        )
    }
}

#[derive(Default)]
struct Buffer {
    entries: VecDeque<Entry>,
    revision: u64,
}

struct Capture {
    inner: env_logger::Logger,
    start: Instant,
}

impl log::Log for Capture {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        self.inner.enabled(metadata)
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.inner.matches(record) {
            return;
        }
        if let Some(buffer) = BUFFER.get()
            && let Ok(mut buffer) = buffer.try_lock()
        {
            if buffer.entries.len() == CAPACITY {
                buffer.entries.pop_front();
            }
            buffer.entries.push_back(Entry {
                elapsed: self.start.elapsed(),
                level: record.level(),
                target: record.target().into(),
                message: record.args().to_string(),
            });
            buffer.revision = buffer.revision.wrapping_add(1);
        }
        self.inner.log(record);
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

pub fn init() -> Result<(), log::SetLoggerError> {
    BUFFER.get_or_init(|| Mutex::new(Buffer::default()));
    let inner = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("warn,nodeplayer=info"),
    )
    .build();
    let filter = inner.filter();
    log::set_boxed_logger(Box::new(Capture {
        inner,
        start: Instant::now(),
    }))?;
    log::set_max_level(filter);
    Ok(())
}

pub struct LogDock {
    filter: String,
    minimum: log::LevelFilter,
    follow: bool,
    revision: Option<u64>,
    entries: Vec<Entry>,
}

impl Default for LogDock {
    fn default() -> Self {
        Self {
            filter: String::new(),
            minimum: log::LevelFilter::Trace,
            follow: true,
            revision: None,
            entries: Vec::new(),
        }
    }
}

impl LogDock {
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let buffer = BUFFER.get_or_init(|| Mutex::new(Buffer::default()));
        {
            let buffer = buffer.lock().unwrap_or_else(|e| e.into_inner());
            if self.revision != Some(buffer.revision) {
                self.entries = buffer.entries.iter().cloned().collect();
                self.revision = Some(buffer.revision);
            }
        }
        let mut clear = false;
        let mut copy = false;
        ui.horizontal_wrapped(|ui| {
            ui.label("Show through");
            egui::ComboBox::from_id_salt("log level")
                .selected_text(self.minimum.to_string())
                .show_ui(ui, |ui| {
                    for level in [
                        log::LevelFilter::Error,
                        log::LevelFilter::Warn,
                        log::LevelFilter::Info,
                        log::LevelFilter::Debug,
                        log::LevelFilter::Trace,
                    ] {
                        ui.selectable_value(&mut self.minimum, level, level.to_string());
                    }
                });
            ui.checkbox(&mut self.follow, "Follow");
            copy = ui.button("Copy").clicked();
            clear = ui.button("Clear").clicked();
        });
        ui.add(
            egui::TextEdit::singleline(&mut self.filter)
                .hint_text("Filter logs")
                .desired_width(f32::INFINITY),
        );
        if clear {
            let mut buffer = buffer.lock().unwrap_or_else(|e| e.into_inner());
            buffer.entries.clear();
            buffer.revision = buffer.revision.wrapping_add(1);
            self.entries.clear();
            self.revision = Some(buffer.revision);
        }
        let needle = self.filter.to_lowercase();
        let entries: Vec<_> = self
            .entries
            .iter()
            .filter(|entry| {
                entry.level.to_level_filter() <= self.minimum
                    && (needle.is_empty()
                        || entry.target.to_lowercase().contains(&needle)
                        || entry.message.to_lowercase().contains(&needle))
            })
            .collect();
        if copy {
            ui.ctx().copy_text(
                entries
                    .iter()
                    .map(|entry| entry.line())
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
        ui.small(format!(
            "{} / {} entries · most recent {CAPACITY} retained · time since startup",
            entries.len(),
            self.entries.len()
        ));
        egui::ScrollArea::both()
            .id_salt("log entries")
            .auto_shrink([false, false])
            .stick_to_bottom(self.follow)
            .show(ui, |ui| {
                for entry in entries {
                    let color = match entry.level {
                        log::Level::Error => ui.visuals().error_fg_color,
                        log::Level::Warn => ui.visuals().warn_fg_color,
                        _ => ui.visuals().text_color(),
                    };
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(entry.line()).monospace().color(color),
                        )
                        .selectable(true),
                    );
                }
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::Log;

    #[test]
    fn capture_honors_filter_and_does_not_wait_for_a_viewer_lock() {
        let buffer = BUFFER.get_or_init(|| Mutex::new(Buffer::default()));
        let capture = Capture {
            inner: env_logger::Builder::new()
                .is_test(true)
                .filter_level(log::LevelFilter::Warn)
                .build(),
            start: Instant::now(),
        };
        capture.log(
            &log::Record::builder()
                .level(log::Level::Info)
                .target("capture-filter-test")
                .args(format_args!("hidden info"))
                .build(),
        );
        capture.log(
            &log::Record::builder()
                .level(log::Level::Warn)
                .target("capture-filter-test")
                .args(format_args!("visible warning"))
                .build(),
        );
        let held = buffer.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            held.entries
                .iter()
                .any(|e| e.target == "capture-filter-test" && e.message == "visible warning")
        );
        assert!(
            !held
                .entries
                .iter()
                .any(|e| e.target == "capture-filter-test" && e.message == "hidden info")
        );
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            capture.log(
                &log::Record::builder()
                    .level(log::Level::Error)
                    .target("capture-lock-test")
                    .args(format_args!("busy UI must not block the logging thread"))
                    .build(),
            );
            tx.send(()).unwrap();
        });
        let completed = rx.recv_timeout(Duration::from_secs(5)).is_ok();
        drop(held);
        thread.join().unwrap();
        assert!(completed, "logging waited for the UI's buffer lock");
    }
}
