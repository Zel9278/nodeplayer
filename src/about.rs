//! The About window: who made nodeplayer, its license, and the licenses of what it is built
//! from. The crate list is `assets/licenses.json`, made from `cargo metadata` by the test
//! below (`NODEPLAYER_WRITE_LICENSES=1 cargo test about` writes it again).

use serde::{Deserialize, Serialize};

const LICENSE: &str = include_str!("../LICENSE");
const CRATES: &str = include_str!("../assets/licenses.json");

/// Code built in that doesn't come as a crate: name, what it is for, license, and where from.
const BUNDLED: &[(&str, &str, &str, &str)] = &[
    ("ysfx", "JSFX plugins", "Apache-2.0", "https://github.com/JoepVanlier/ysfx"),
    ("WDL (EEL2, LICE, SWELL)", "used by ysfx", "Zlib", "https://www.cockos.com/wdl/"),
    ("dr_libs", "used by ysfx", "Unlicense OR MIT-0", "https://github.com/mackron/dr_libs"),
    ("stb", "used by ysfx", "Unlicense OR MIT", "https://github.com/nothings/stb"),
    ("winit 0.30.13", "the window; changed for IME input", "Apache-2.0", "https://github.com/rust-windowing/winit"),
];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Crate {
    name: String,
    version: String,
    license: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    repository: Option<String>,
}

/// The crates nodeplayer names in its Cargo.toml (on any platform), and every crate built
/// into it.
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
struct Crates {
    direct: Vec<Crate>,
    all: Vec<Crate>,
}

#[derive(Default)]
pub struct About {
    pub open: bool,
    crates: Option<Crates>,
}

impl About {
    pub fn show(&mut self, ctx: &egui::Context) {
        if !self.open {
            return;
        }
        let crates = self.crates.get_or_insert_with(|| {
            serde_json::from_str(CRATES).unwrap_or_else(|e| {
                log::error!("licenses.json: {e}");
                Crates::default()
            })
        });
        egui::Window::new("About nodeplayer")
            .open(&mut self.open)
            .default_size([560.0, 520.0])
            .show(ctx, |ui| {
                ui.heading(format!("nodeplayer {}", env!("CARGO_PKG_VERSION")));
                ui.label(format!("Created by {}", env!("CARGO_PKG_AUTHORS")));
                ui.label("Released under the MIT License.");
                ui.add_space(6.0);
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.collapsing("License", |ui| {
                        ui.label(egui::RichText::new(LICENSE).monospace().small());
                    });
                    egui::CollapsingHeader::new(format!("Libraries ({})", crates.direct.len()))
                        .default_open(true)
                        .show(ui, |ui| crate_grid(ui, "direct", &crates.direct));
                    egui::CollapsingHeader::new("Code built in").default_open(true).show(ui, |ui| {
                        egui::Grid::new("bundled").num_columns(3).striped(true).show(ui, |ui| {
                            for (name, what, license, url) in BUNDLED {
                                ui.hyperlink_to(*name, *url);
                                ui.weak(*what);
                                ui.label(*license);
                                ui.end_row();
                            }
                        });
                    });
                    ui.collapsing(format!("Every crate in the build ({})", crates.all.len()), |ui| {
                        crate_grid(ui, "all", &crates.all);
                    });
                    ui.add_space(4.0);
                    ui.weak(
                        "FFmpeg isn't part of nodeplayer: it runs as its own programs (ffmpeg, ffprobe) \
                         when they are installed, under the license of that build (LGPL or GPL).",
                    );
                });
            });
    }
}

fn crate_grid(ui: &mut egui::Ui, id: &str, crates: &[Crate]) {
    egui::Grid::new(id).num_columns(3).striped(true).show(ui, |ui| {
        for c in crates {
            match &c.repository {
                Some(url) => ui.hyperlink_to(&c.name, url),
                None => ui.label(&c.name),
            };
            ui.weak(&c.version);
            ui.label(&c.license);
            ui.end_row();
        }
    });
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashSet};

    use super::*;

    /// The crate list as `cargo metadata` has it now.
    fn from_metadata() -> Crates {
        let dir = env!("CARGO_MANIFEST_DIR");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let out = std::process::Command::new(cargo)
            .args(["metadata", "--format-version", "1", "--locked"])
            .current_dir(dir)
            .output()
            .expect("run cargo metadata");
        assert!(out.status.success(), "cargo metadata: {}", String::from_utf8_lossy(&out.stderr));
        let m: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let packages: BTreeMap<&str, &serde_json::Value> =
            m["packages"].as_array().unwrap().iter().map(|p| (p["id"].as_str().unwrap(), p)).collect();
        let nodes: BTreeMap<&str, &serde_json::Value> =
            m["resolve"]["nodes"].as_array().unwrap().iter().map(|n| (n["id"].as_str().unwrap(), n)).collect();
        // What is linked in: normal dependencies, not build or dev ones.
        let linked = |id: &str| -> Vec<&str> {
            nodes[id]["deps"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|d| d["dep_kinds"].as_array().unwrap().iter().any(|k| k["kind"].is_null()))
                .map(|d| d["pkg"].as_str().unwrap())
                .collect()
        };
        let entry = |id: &str| {
            let p = packages[id];
            Crate {
                name: p["name"].as_str().unwrap().into(),
                version: p["version"].as_str().unwrap().into(),
                license: p["license"].as_str().unwrap_or("see its repository").into(),
                repository: p["repository"].as_str().map(str::to_owned),
            }
        };
        let sorted = |mut v: Vec<Crate>| {
            v.sort_by(|a, b| (&a.name, &a.version).cmp(&(&b.name, &b.version)));
            v
        };
        let root = packages
            .iter()
            .find(|(_, p)| p["name"] == "nodeplayer" && p["source"].is_null())
            .map(|(id, _)| *id)
            .unwrap();
        let direct = sorted(linked(root).into_iter().map(entry).collect());
        let mut seen = HashSet::new();
        let mut stack = linked(root);
        while let Some(id) = stack.pop() {
            if seen.insert(id) {
                stack.extend(linked(id));
            }
        }
        let all = sorted(seen.into_iter().map(entry).map(|c| Crate { repository: None, ..c }).collect());
        Crates { direct, all }
    }

    #[test]
    fn about_crate_list_is_current() {
        let now = from_metadata();
        if std::env::var_os("NODEPLAYER_WRITE_LICENSES").is_some() {
            let path = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/licenses.json");
            std::fs::write(path, serde_json::to_string_pretty(&now).unwrap() + "\n").unwrap();
            return;
        }
        let shipped: Crates = serde_json::from_str(CRATES).unwrap();
        assert!(
            shipped == now,
            "assets/licenses.json is out of date: run NODEPLAYER_WRITE_LICENSES=1 cargo test about"
        );
    }
}
