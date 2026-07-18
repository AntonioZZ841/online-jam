//! egui session window (behind the `gui` cargo feature): the graphical
//! counterpart of the terminal status display. Reads the same 500 ms
//! snapshot and writes the same gain atomics — no protocol or audio code is
//! touched, so the GUI can never affect the real-time path beyond what the
//! terminal UI already does.

use crate::status_ui::UiShared;
use jam_core::stats::{Role, SharedSnapshot, Snapshot};
use jam_core::AtomicF32;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub fn run(
    title: &str,
    snapshot: SharedSnapshot,
    shared: UiShared,
    xruns: Arc<AtomicU64>,
) -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([680.0, 420.0])
            .with_min_inner_size([520.0, 300.0]),
        ..Default::default()
    };
    let app = JamApp {
        snapshot,
        shared,
        xruns,
    };
    eframe::run_native(
        title,
        options,
        Box::new(move |_cc| Ok(Box::new(app) as Box<dyn eframe::App>)),
    )
    .map_err(|e| anyhow::anyhow!("gui: {e}"))
}

struct JamApp {
    snapshot: SharedSnapshot,
    shared: UiShared,
    xruns: Arc<AtomicU64>,
}

fn db_of(gain: &AtomicF32) -> f32 {
    20.0 * gain.get().max(1e-3).log10()
}

fn set_db(gain: &AtomicF32, db: f32) {
    gain.set(10f32.powf(db.clamp(-40.0, 12.0) / 20.0));
}

fn gain_slider(ui: &mut eframe::egui::Ui, label: &str, gain: &AtomicF32) {
    let mut db = db_of(gain);
    let slider = eframe::egui::Slider::new(&mut db, -40.0..=12.0)
        .suffix(" dB")
        .fixed_decimals(1);
    ui.label(label);
    if ui.add(slider).changed() {
        set_db(gain, db);
    }
}

fn level_bar(ui: &mut eframe::egui::Ui, rms_db: f32, peak_db: f32) {
    let frac = ((rms_db + 60.0) / 60.0).clamp(0.0, 1.0);
    let bar = eframe::egui::ProgressBar::new(frac)
        .desired_width(160.0)
        .text(format!("{rms_db:>5.1} dB (pk {peak_db:>5.1})"));
    ui.add(bar);
}

impl eframe::App for JamApp {
    fn update(&mut self, ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {
        // The snapshot refreshes twice a second; poll a little faster so
        // slider interaction stays smooth.
        ctx.request_repaint_after(Duration::from_millis(250));
        let snap: Snapshot = self.snapshot.lock().unwrap().clone();

        eframe::egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading(&snap.status);
            ui.label(format!(
                "est. mouth-to-ear ≈ {:.1} ms    xruns {}    capture gaps {}",
                snap.est_latency_ms,
                self.xruns.load(Ordering::Relaxed),
                snap.capture_starved,
            ));
            ui.separator();

            eframe::egui::Grid::new("players")
                .striped(true)
                .min_col_width(60.0)
                .show(ui, |ui| {
                    ui.strong("player");
                    ui.strong("level");
                    ui.strong("loss");
                    ui.strong("jitter");
                    ui.strong("buffer");
                    ui.strong("rtt");
                    if matches!(self.shared, UiShared::Host(_)) {
                        ui.strong("gain");
                    }
                    ui.end_row();

                    for p in &snap.players {
                        ui.label(&p.name);
                        level_bar(ui, p.rms_db, p.peak_db);
                        ui.label(format!("{:.1}%", p.loss * 100.0));
                        ui.label(format!("{:.1} ms", p.jitter_ms));
                        ui.label(format!("{:.1} ms", p.buffer_ms));
                        ui.label(format!("{:.1} ms", p.rtt_ms));
                        if let UiShared::Host(h) = &self.shared {
                            if let Some(gain) = h.gains.get(p.id as usize) {
                                let mut db = db_of(gain);
                                let slider = eframe::egui::Slider::new(&mut db, -40.0..=12.0)
                                    .suffix(" dB")
                                    .fixed_decimals(1);
                                if ui.add(slider).changed() {
                                    set_db(gain, db);
                                }
                            }
                        }
                        ui.end_row();
                    }
                });

            ui.separator();
            match &self.shared {
                UiShared::Host(h) => {
                    ui.horizontal(|ui| {
                        gain_slider(ui, "self monitor", &h.monitor_gain);
                    });
                }
                UiShared::Client(c) => {
                    ui.horizontal(|ui| {
                        gain_slider(ui, "master", &c.master_gain);
                        ui.separator();
                        gain_slider(ui, "monitor", &c.monitor_gain);
                    });
                }
            }
            if snap.role == Role::Client && !snap.connected {
                ui.separator();
                ui.colored_label(
                    eframe::egui::Color32::YELLOW,
                    "not connected — see status above",
                );
            }
        });
    }
}
