//! Tap applications by hand and watch what arrives.
//!
//! ```sh
//! cargo run -p simply-audio-capture --example app_tap -- --list
//! cargo run -p simply-audio-capture --example app_tap -- com.algoriddim.djay-iphone-free
//! cargo run -p simply-audio-capture --example app_tap -- --mute "djay Pro"
//! cargo run -p simply-audio-capture --example app_tap -- --except            # everything but this
//! ```
//!
//! Prints the status and the peak level once a second until Ctrl-C. The
//! first tap may raise macOS's System Audio Recording prompt.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use simply_audio_capture::{self as capture, App, AppTap, Mute};

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--list") {
        for a in capture::list_apps() {
            let mark = if a.playing { "▶" } else { " " };
            let this = if a.this { " (this)" } else { "" };
            println!("{mark} {:>6}  {:<28} {}{this}", a.pid, a.name, a.app());
        }
        return;
    }
    let take = |flag: &str, args: &mut Vec<String>| {
        let had = args.iter().any(|a| a == flag);
        args.retain(|a| a != flag);
        had
    };
    let mute = if take("--mute", &mut args) { Mute::Muted } else { Mute::Unmuted };
    let except = take("--except", &mut args);
    let apps: Vec<App> = args.iter().map(|a| App::parse(a)).collect();
    let tap = if except || apps.is_empty() {
        AppTap::all_except(apps.into_iter().chain([App::This]))
    } else {
        AppTap::only(apps)
    }
    .with_mute(mute);

    let peak = Arc::new(AtomicU32::new(0));
    let sink = {
        let peak = Arc::clone(&peak);
        move |frames: &[f32], _ns: u64| {
            let p = frames.iter().fold(0f32, |m, s| m.max(s.abs()));
            peak.fetch_max(p.to_bits(), Ordering::Relaxed);
        }
    };
    let capture = match capture::start_app(&tap, 48_000, 2, sink) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("app_tap: {e}");
            std::process::exit(1);
        }
    };
    println!("{:?} ({:?})", capture.tap().apps, capture.tap().mute);
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let p = f32::from_bits(peak.swap(0, Ordering::Relaxed));
        let db = if p > 0.0 { format!("{:6.1} dBFS", 20.0 * p.log10()) } else { "  silent".into() };
        println!("{db}  {}", capture.status());
    }
}
