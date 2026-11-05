//! Tap applications each on its own channel, and watch them come and go.
//!
//! ```sh
//! cargo run -p simply-audio-capture --example app_channels                     # every app but this
//! cargo run -p simply-audio-capture --example app_channels -- Music Spotify    # these apps only
//! ```
//!
//! Prints a line as each app's channel starts and stops, and once a second
//! every channel with its peak level, until Ctrl-C. The first tap may raise
//! macOS's System Audio Recording prompt.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use simply_audio_capture::{self as capture, App, AppInfo, AppSinks, AppTap};

/// One channel's peak since the last print (an f32's bits).
type Peak = Arc<AtomicU32>;

/// Each channel's peak, by pid.
#[derive(Clone, Default)]
struct Peaks(Arc<Mutex<BTreeMap<i32, Peak>>>);

impl AppSinks for Peaks {
    type Sink = Box<dyn FnMut(&[f32], u64) + Send>;

    fn started(&mut self, app: &AppInfo) -> Self::Sink {
        println!("+ {} (pid {})", app.name, app.pid);
        let peak = Arc::new(AtomicU32::new(0));
        self.0.lock().unwrap().insert(app.pid, Arc::clone(&peak));
        Box::new(move |frames: &[f32], _ns: u64| {
            let p = frames.iter().fold(0f32, |m, s| m.max(s.abs()));
            peak.fetch_max(p.to_bits(), Ordering::Relaxed);
        })
    }

    fn stopped(&mut self, app: &AppInfo) {
        println!("- {} (pid {})", app.name, app.pid);
        self.0.lock().unwrap().remove(&app.pid);
    }
}

fn main() {
    let apps: Vec<App> = std::env::args().skip(1).map(|a| App::parse(&a)).collect();
    let tap = if apps.is_empty() { AppTap::all_except([App::This]) } else { AppTap::only(apps) };
    let peaks = Peaks::default();
    let capture = match capture::start_app_channels(&tap, 48_000, 2, peaks.clone()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("app_channels: {e}");
            std::process::exit(1);
        }
    };
    println!("{:?}", capture.tap().apps);
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let status = capture.status();
        let peaks = peaks.0.lock().unwrap();
        for c in &status.channels {
            let p =
                peaks.get(&c.app.pid).map_or(0.0, |p| f32::from_bits(p.swap(0, Ordering::Relaxed)));
            let db =
                if p > 0.0 { format!("{:6.1} dBFS", 20.0 * p.log10()) } else { "  silent".into() };
            println!("  {db}  {:>6}  {:<28} {:?}", c.app.pid, c.app.name, c.state);
        }
        if !status.missing.is_empty() {
            println!("  missing: {}", status.missing.join(", "));
        }
        println!();
    }
}
