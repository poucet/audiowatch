#### Each app on its own channel · 2026-11-05 05:16 · lnlnvopq

`simply-audio-capture` 0.1.3 can tap applications **one channel per app** instead of one stereo mix: `start_app_channels` takes the same `AppTap` as `start_app` (these apps, or everything except some) and gives every process it selects a tap of its own, from when it starts playing until it exits or has been quiet for 5 seconds. The caller hears each channel start and stop and hands each its own sink, so it can meter and mix the apps apart. `start_app` and its one mix are unchanged. `cargo run -p simply-audio-capture --example app_channels` shows the channels live.

#### A paused app's tap is kept · 2026-11-05 05:19 · wzmstlms

An app tap (`start_app`) whose apps were all paused used to be torn down and re-made every 2 seconds, because a tap of processes that play nothing gets no callbacks and that read as a stall. It now stays put until something plays again, so a paused app no longer churns the Mac's audio devices.
