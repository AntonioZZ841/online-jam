//! Terminal status display + keyboard control, refreshed twice a second.
//!
//! Keys (host): 0-4 select player, +/- adjust their gain, m mute, s solo,
//! q quit.
//! Keys (client): +/- master volume, [/] monitor volume, q quit.

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::{cursor, execute, terminal};
use jam_core::stats::{Role, SharedSnapshot, Snapshot};
use jam_core::{ClientShared, HostShared};
use std::io::Write;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub enum UiShared {
    Host(Arc<HostShared>),
    Client(Arc<ClientShared>),
}

struct RawMode;

impl RawMode {
    fn enter() -> Option<Self> {
        terminal::enable_raw_mode().ok().map(|_| RawMode)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

fn meter_bar(rms_db: f32, peak_db: f32) -> String {
    const WIDTH: usize = 20;
    let scale = |db: f32| (((db + 60.0) / 60.0).clamp(0.0, 1.0) * WIDTH as f32) as usize;
    let rms_w = scale(rms_db);
    let peak_w = scale(peak_db).max(rms_w);
    let mut bar = String::with_capacity(WIDTH);
    for i in 0..WIDTH {
        bar.push(if i < rms_w {
            '█'
        } else if i < peak_w {
            '▒'
        } else {
            '·'
        });
    }
    bar
}

fn render(
    snap: &Snapshot,
    selected: usize,
    gains: &[f32],
    muted: &[bool],
    soloed: &[bool],
    xruns: u64,
) -> String {
    let is_host = snap.role == Role::Host;
    let any_solo = soloed.iter().any(|&s| s);
    let mut out = String::new();
    out.push_str(&format!("  {}\r\n", snap.status));
    out.push_str(&format!(
        "  est. mouth-to-ear ≈ {:>5.1} ms   xruns {}   capture gaps {}\r\n\r\n",
        snap.est_latency_ms, xruns, snap.capture_starved
    ));
    out.push_str(
        "     player            level                  loss    jitter  buffer   rtt   gain  m/s\r\n",
    );
    for p in &snap.players {
        let idx = p.id as usize;
        let sel = if is_host && idx == selected { '>' } else { ' ' };
        let gain = gains
            .get(idx)
            .map(|g| format!("{:>4.1}", 20.0 * g.max(1e-3).log10()))
            .unwrap_or_else(|| "  - ".into());
        // Mute/solo markers, host only. A player silenced by someone else's
        // solo shows a dim 's' so it's clear why they've gone quiet.
        let flags = if is_host {
            let m = if muted.get(idx).copied().unwrap_or(false) {
                'M'
            } else {
                '·'
            };
            let s = if soloed.get(idx).copied().unwrap_or(false) {
                'S'
            } else if any_solo {
                's'
            } else {
                '·'
            };
            format!(" {m}{s}")
        } else {
            String::new()
        };
        out.push_str(&format!(
            "  {sel} {:<2} {:<12} [{}] {:>5.1}%  {:>5.1}ms {:>5.1}ms {:>5.1}ms  {gain}dB {flags}\r\n",
            p.id,
            truncate(&p.name, 12),
            meter_bar(p.rms_db, p.peak_db),
            p.loss * 100.0,
            p.jitter_ms,
            p.buffer_ms,
            p.rtt_ms,
        ));
    }
    out.push_str("\r\n");
    match snap.role {
        Role::Host => {
            out.push_str("  keys: 0-4 select player · +/- gain · m mute · s solo · q quit\r\n")
        }
        Role::Client => out.push_str("  keys: +/- master volume · [/] monitor volume · q quit\r\n"),
    }
    out
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n - 1).chain(std::iter::once('…')).collect()
    }
}

fn adjust_db(current: f32, steps: f32) -> f32 {
    let db = 20.0 * current.max(1e-3).log10() + steps;
    10f32.powf(db.clamp(-40.0, 12.0) / 20.0)
}

/// Runs until `q`/Ctrl-C. Returns when the user asks to quit.
pub fn run(
    snapshot: &SharedSnapshot,
    shared: &UiShared,
    xruns: &std::sync::atomic::AtomicU64,
) -> anyhow::Result<()> {
    let _raw = RawMode::enter();
    let mut stdout = std::io::stdout();
    let _ = execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide);
    let mut selected = 0usize;
    let mut last_draw = Instant::now() - Duration::from_secs(1);

    let result = loop {
        // Keyboard (poll fast so keys feel immediate).
        if event::poll(Duration::from_millis(50)).unwrap_or(false) {
            if let Ok(Event::Key(key)) = event::read() {
                let quit = matches!(key.code, KeyCode::Char('q'))
                    || (key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL));
                if quit {
                    break Ok(());
                }
                match (&shared, key.code) {
                    (UiShared::Host(h), KeyCode::Char(c @ '0'..='4')) => {
                        let idx = c as usize - '0' as usize;
                        // Only the host (0) or a currently-active client slot
                        // is selectable, so gain/mute/solo keys can't land on
                        // an empty slot.
                        let selectable = idx == 0
                            || h.clients
                                .get(idx - 1)
                                .is_some_and(|s| s.active.load(Ordering::Acquire));
                        if selectable {
                            selected = idx;
                        }
                    }
                    (UiShared::Host(h), KeyCode::Char('+' | '=')) => {
                        let g = &h.gains[selected];
                        g.set(adjust_db(g.get(), 1.0));
                    }
                    (UiShared::Host(h), KeyCode::Char('-')) => {
                        let g = &h.gains[selected];
                        g.set(adjust_db(g.get(), -1.0));
                    }
                    (UiShared::Host(h), KeyCode::Char('m')) => {
                        let f = &h.muted[selected];
                        f.store(!f.load(Ordering::Relaxed), Ordering::Relaxed);
                    }
                    (UiShared::Host(h), KeyCode::Char('s')) => {
                        let f = &h.soloed[selected];
                        f.store(!f.load(Ordering::Relaxed), Ordering::Relaxed);
                    }
                    (UiShared::Client(c), KeyCode::Char('+' | '=')) => {
                        c.master_gain.set(adjust_db(c.master_gain.get(), 1.0));
                    }
                    (UiShared::Client(c), KeyCode::Char('-')) => {
                        c.master_gain.set(adjust_db(c.master_gain.get(), -1.0));
                    }
                    (UiShared::Client(c), KeyCode::Char(']')) => {
                        c.monitor_gain.set(adjust_db(c.monitor_gain.get(), 1.0));
                    }
                    (UiShared::Client(c), KeyCode::Char('[')) => {
                        c.monitor_gain.set(adjust_db(c.monitor_gain.get(), -1.0));
                    }
                    _ => {}
                }
                last_draw -= Duration::from_secs(1); // redraw immediately
            }
        }

        if last_draw.elapsed() >= Duration::from_millis(500) {
            last_draw = Instant::now();
            // If the selected client has since left, fall back to the host row
            // so the selector never points at an empty slot.
            if let UiShared::Host(h) = &shared {
                if selected != 0
                    && !h
                        .clients
                        .get(selected - 1)
                        .is_some_and(|s| s.active.load(Ordering::Acquire))
                {
                    selected = 0;
                }
            }
            let snap = snapshot.lock().unwrap().clone();
            let (gains, muted, soloed): (Vec<f32>, Vec<bool>, Vec<bool>) = match &shared {
                UiShared::Host(h) => (
                    h.gains.iter().map(|g| g.get()).collect(),
                    h.muted.iter().map(|f| f.load(Ordering::Relaxed)).collect(),
                    h.soloed.iter().map(|f| f.load(Ordering::Relaxed)).collect(),
                ),
                UiShared::Client(c) => (vec![c.master_gain.get()], vec![], vec![]),
            };
            let body = render(
                &snap,
                selected,
                &gains,
                &muted,
                &soloed,
                xruns.load(Ordering::Relaxed),
            );
            let _ = execute!(
                stdout,
                cursor::MoveTo(0, 0),
                terminal::Clear(terminal::ClearType::All)
            );
            let _ = write!(stdout, "\r\n{body}");
            let _ = stdout.flush();
        }
    };

    let _ = execute!(stdout, terminal::LeaveAlternateScreen, cursor::Show);
    result
}
