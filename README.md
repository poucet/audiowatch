# audiowatch

Names the process that just put audio on one of this Mac's outputs.

* **Short-lived ones too**: a process that starts, plays and exits in a fraction of a second.
* **Silent itself**: it never plays anything.

## Run it

```sh
cargo build --release
./target/release/audiowatch
```

* Watches until you stop it.
* Prints events.
* Posts a notification for anything not on the allow-list.

### Running it all the time

```sh
./target/release/audiowatch --install-agent   # writes the LaunchAgent, prints the command
```

* **It does not load the agent.** Deliberately.
  * It prints the `launchctl bootstrap` line for you to run.
* **Copy the binary somewhere stable first**, such as `/usr/local/bin`.
  * The plist points at wherever the binary was when you ran the command.

## Everything else it does

```sh
audiowatch --tail            # the last 20 sound events
audiowatch --tail 100 -f     # ... and keep printing new ones
audiowatch --since 1h        # what made sound in the last hour
audiowatch --since 2026-09-26T11:00
audiowatch --all --tail      # include the connect/disconnect chatter
audiowatch --now             # what is making sound at this instant
audiowatch --devices         # devices and their channel counts
audiowatch --test-notify     # post a notification and verify it arrived
audiowatch --paths           # where the config and log are
```

## Where things live

| | |
|---|---|
| log | `~/Library/Logs/audiowatch/audiowatch.log` |
| config | `~/.config/audiowatch/config.conf` |
| agent | `~/Library/LaunchAgents/local.audiowatch.plist` |

### The log

* **The log is the primary output.**
* Each line is written and `fsync`ed **before** the notification is attempted.
  * A missed, dismissed or suppressed notification never loses the record.
* **Format**: tab-separated, backslash-escaped, one line per event.

```
epoch_ms  local_time  kind  disposition  pid  bundle  exe  devices  note
```

* **`kind`**: one of `connect`, `output-start`, `output-stop`, `input-start`, `input-stop`, `disconnect`, `baseline`.
* **Rotation**: to `audiowatch.log.1` at 8 MB.

## Stopping something from notifying you

Add one line to `~/.config/audiowatch/config.conf`:

```
allow-exe    afplay                      # match the executable's file name
allow-path   /Applications/Thing.app/*   # match the full executable path
allow-bundle com.example.thing           # match the bundle id
```

* Patterns are case-insensitive.
  * `*` matches anything. `?` matches one character.
* Running as an agent? Restart it: `launchctl kickstart -k gui/$(id -u)/local.audiowatch`.

### Allowed is not hidden

**An allow-listed event is still logged**, with the rule that suppressed it. You can always go back and see what was hidden:

```
11:43:30  output-start  Discord Helper  pid 81645  dev[Speakers] (allowed by path /Applications/Discord.app/*)
```

### The default list

* **Covers**: Chrome, Safari, Firefox, Arc, Brave, Edge, Spotify, Music, TV, Podcasts, QuickTime, VLC, IINA, Zoom, Slack, Discord, Teams, FaceTime, Messages and WhatsApp.
* Matched on bundle id *and* on path.
  * A helper process usually has a path and no bundle id.
* The config file lists them all, with comments.
  * It is the same text that ships as the defaults.

## How it detects the start of output

Three CoreAudio facts. Each was measured on this machine (macOS 26.6.2), not taken from documentation.

| API | Sends notifications? |
|---|---|
| `kAudioHardwarePropertyProcessObjectList` | **yes**, within ~1 ms of a process connecting to the HAL |
| `kAudioDevicePropertyDeviceIsRunningSomewhere` | **yes**, when IO starts on a device. It cannot say which process |
| `kAudioProcessPropertyIsRunningOutput` | **no.** A listener registers successfully (`status 0`) and then never fires |

**No single API answers the question.** So the tool combines all three:

```mermaid
flowchart LR
  C[HAL connect<br/>ProcessObjectList listener] -->|pid, path, bundle id cached| E[output-start]
  C -->|always| L[connect logged]
  D[device IO starts<br/>DeviceIsRunningSomewhere] -->|poll at 10 ms for 2 s| P
  P[IsRunningOutput<br/>polled every 50 ms] -->|0 to 1| E
```

1. **Identity is captured at the HAL connect**, by the process object list listener.
   * A process connects *before* it makes a sound: measured at 112–220 ms before.
   * It is **necessarily** still alive at that moment.
   * So its pid, executable path and bundle id are read and cached right then.
   * That is what lets an `output-start` name a process that has since exited.
2. **`IsRunningOutput` is polled** every 50 ms, because it does not notify.
   * This is what actually detects output starting.
3. **A device reporting IO** drops the poll to 10 ms for 2 seconds.
   * That pins down which process was responsible.
4. **Every HAL connect is logged**, even if output is never confirmed.
   * A process cannot make a sound without connecting first. This is the backstop.
   * Worst case: "`/usr/bin/afplay` connected at 11:43:32.340 and was gone 214 ms later". Still the clue you needed.

### Notifications

* Posted through `osascript`'s `display notification`.
  * Needs no app bundle of our own.
  * Works from a LaunchAgent.
* ✅ **Verified** by posting one: `usernoted` logged the presentation 130 ms later.
* They carry **no sound**.
* `audiowatch --test-notify` repeats that check.
  * If it fails, it points you at System Settings → Notifications → **Script Editor**.
  * That is the app notifications are attributed to.

### What it costs

* Measured on this machine: **0.7% of one core and 13 MB resident**, at the default 50 ms poll.
* An idle tick is two property reads per audio process. Nothing else.
* Identity and device lookups happen only when a flag actually moves.
  * That is what keeps a permanently-resident watcher cheap.

## What still gets past it

⚠️ Read this before trusting it.

* **A process that holds an output stream open permanently.**
  * Detection is on the *transition* of `IsRunningOutput` from 0 to 1.
  * Open at login, kept open, mostly silence, occasionally audible: it never transitions and is never reported.
  * On this machine `arkaudiod` (Audio Routing Kit) is exactly this shape.
    * Permanently running output on MacBook Pro Speakers, both BlackHole devices and the LG display.
  * **If the noise arrives through ARK, Loopback, BlackHole or JACK, audiowatch will attribute it to the bridge, or not see it at all.**
  * These are deliberately *not* on the default allow-list.
  * The lever: a process tap, below.
* **Sounds played on another process's behalf.**
  * `systemsoundserverd` plays UI and alert sounds for other apps.
  * You see `systemsoundserverd`, not whoever asked.
  * Same for `PowerChime` (the charger sound) and `audiomxd`.
* **Anything that is not a HAL client process at all.**
  * The boot chime, which is firmware.
  * Audio generated inside a HAL plug-in driver rather than by a client.
* **Which channels.**
  * CoreAudio's per-process API reports the *devices* a process is running on.
  * It has no per-channel attribution.
  * So audiowatch names the device, and `--devices` gives its channel count.
  * It cannot say "channels 3–4 of the Scarlett".
* **A gap shorter than the poll.**
  * An output stream open for less than 50 ms **possibly** falls between two polls.
  * In practice the window is far longer than the sound: a process opens the device, plays, and lingers.
  * Across 13 measured `afplay` runs of 100–500 ms files, the shortest output window was **524 ms**.
    * Ten times the poll interval.
  * Want more margin? Lower `poll-ms`.
* **A device something else already holds.**
  * `DeviceIsRunningSomewhere` is a per-device flag, not a per-process one.
  * When `arkaudiod` already has MacBook Pro Speakers running, a second process starting output there changes nothing.
    * No notification arrives.
  * On this machine several devices are permanently running.
  * **So the poll is the real detector. The device listener is only an accelerator.**
  * Do not raise `poll-ms` far on the assumption that the listener will cover it.
* **A process that exits before its path can be read.**
  * The pid and bundle id are still logged.
  * The note says the path was never readable.

### The escalation: a process tap

* If the watcher comes up empty, the next step is a **process tap**.
  * `kAudioTapClassID` / `CATapDescription`, in the same headers.
* It measures the actual signal a process produces, not whether it is running IO.
* So it would see audio passing through a permanently-open stream.
  * That is the gap above that matters most.
* Started, not finished: see [Status](#status-done-2026-09-26).

## Layout

| | |
|---|---|
| `src/sys.rs` | CoreAudio / libproc / libc declarations, checked against the SDK headers |
| `src/hal.rs` | HAL queries, behind the `HalView` trait |
| `src/reconcile.rs` | the state machine: HAL snapshot in, events out. Pure, so it is tested against a fake HAL |
| `src/watch.rs` | listeners, the poll loop, and what to do with an event |
| `src/event.rs` | the log record and its encoding |
| `src/filter.rs` | the allow-list and its glob matching |
| `src/config.rs` | the config file; `src/default_config.conf` is both the docs and the defaults |
| `src/logfile.rs` | append-only writer, reader and `--since` / `--tail` |
| `src/notify.rs` | notifications, and checking they arrived |
| `src/agent.rs` | the LaunchAgent plist |

* **No dependencies.**
* `cargo test` covers the encoding, the filter rules, the log reader, the CLI and the time parsing.
  * Also the whole state machine, including the short-lived-process case.
* The CoreAudio callbacks are covered by hand, as described above.

## The process tree

* **`afplay` on its own is meaningless.** Half the machine plays a sound that way.
* **`afplay ← zsh ← claude` is the whole answer.**
* So every record carries the chain above the process.
* It is walked **when the process connects to the HAL**, not when its output is noticed.
  * A process connects 113–321 ms before it makes a sound.
  * By the time a 200 ms burst is seen, its parents can be gone too.

```
12:10:39  output-start  afplay   pid 87533  ← zsh ← claude  dev[Speakers]
```

* Nearest ancestor first, stopping at `launchd`.
* Depth-capped at 12, with a cycle guard.
* **The short form shows three.**
  * The log line carries the whole chain, with every path.
  * `--now` prints all of it.

### `allow-ancestor`

The filter can match **any ancestor**, by file name or by path:

```conf
allow-ancestor claude          # anything my agent sessions spawn
allow-ancestor /opt/homebrew/* # or by where it lives
```

* **This rule is what makes the watcher useful.**
* `afplay` is how everything plays a sound.
  * Excusing `afplay` would hide everything, including the noise you built this to find.
  * Excusing *what started it* does not.
* An ancestor whose path could not be read matches no rule.
  * A bare pid is not an identity to trust a rule against.

## Status: done (2026-09-26)

Chris: *"audiowatch can be considered done."*

* ✅ **Done**: the detector, the log, the filter, the notifications and the process tree.
  * Finished, gated and installed at `~/.local/bin/audiowatch`.
* **Nothing is running.**
  * The LaunchAgent is written and **not loaded**.
  * It does nothing until you bootstrap it.

```sh
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/local.audiowatch.plist
launchctl bootout   gui/$(id -u)/local.audiowatch    # and to stop it
```

### Not yet constructed: the process tap

* **The process tap was started and is not finished.**
* `src/tap.rs` and `src/bridge.rs` are in the tree.
  * Unreferenced, marked `#[allow(dead_code)]` item by item.
* They are the escalation described in [What still gets past it](#what-still-gets-past-it).
* They would see audio passing through a **permanently open** stream.
  * The one failure mode this tool cannot detect.
  * The shape `arkaudiod` has on this machine.
* Nothing else depends on them.
* Kept rather than deleted, so that route is a resumption, not a rewrite.
