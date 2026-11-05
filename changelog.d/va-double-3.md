#### No more doubling when an app moves to or shares a virtual device · 2026-11-05 05:55 · vssnwxky

An "everything except" app tap that leaves out `App::PlayingInto` devices (Tussen's) now notices an app switching device the moment it happens: the capture listens to each process's devices instead of waiting for its half-second poll, so the up to 500 ms where the app was heard twice is gone (measured: re-tapped 15–18 ms after the switch). An app playing into such a device **and** the speakers at once used to be left out entirely. Now only its output into the left-out device is dropped, and what it plays to the speakers is still captured through a tap scoped to that device. This holds for the one-mix `start_app` and for `start_app_channels`, where the app keeps its channel. Hand check: `cargo test -p simply-audio-capture two_devices -- --ignored --nocapture` (needs a second output device, such as BlackHole).

#### A new app’s channel starts at once · 2026-11-05 06:05 · pumvnktt

`start_app_channels` used to notice an app starting or stopping play only on its half-second poll, so a new app's channel could start up to 500 ms late. The capture now listens for each process starting and stopping play, and the channel starts as soon as the app does. This covers `start_app` too.

#### "tapping everything" · 2026-11-05 06:05 · ooypyrtu

An "everything except" app tap that leaves nothing out used to report `tapping  at 44100 Hz`. It now reports `tapping everything at 44100 Hz`.

#### An all-except status says what it leaves out · 2026-11-05 06:09 · ssrquoty

An "everything except" app tap reported the processes it left out as if they were tapped (`tapping djay Pro (pid 412) at 44100 Hz`). It now reads `tapping everything except djay Pro (pid 412) at 44100 Hz`. `AppStatus::Tapping` gains `all_except: bool`, which says which kind of tap it is. Code that builds that variant, or matches it without `..`, needs the new field.
