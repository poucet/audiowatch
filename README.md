# audiowatch

Tells you which process just put audio on one of this Mac's outputs — including
a process that starts, plays and exits in a fraction of a second.

It never plays anything itself.

## Run it

```sh
cargo build --release
./target/release/audiowatch
```

That watches until you stop it, printing events and posting a notification for
anything that is not on the allow-list. To have it running all the time:

```sh
./target/release/audiowatch --install-agent   # writes the LaunchAgent, prints the command
```

It deliberately does not load the agent — it prints the `launchctl bootstrap`
line for you to run. Copy the binary somewhere stable first (`/usr/local/bin`),
because the plist points at wherever the binary was when you ran the command.

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

The log is the primary output. Each line is written and `fsync`ed **before** the
notification is attempted, so a missed, dismissed or suppressed notification
never loses the record. Tab-separated, backslash-escaped, one line per event:

```
epoch_ms  local_time  kind  disposition  pid  bundle  exe  devices  note
```

`kind` is one of `connect`, `output-start`, `output-stop`, `input-start`,
`input-stop`, `disconnect`, `baseline`. It rotates to `audiowatch.log.1` at 8 MB.

## Stopping something from notifying you

Add one line to `~/.config/audiowatch/config.conf`:

```
allow-exe    afplay                      # match the executable's file name
allow-path   /Applications/Thing.app/*   # match the full executable path
allow-bundle com.example.thing           # match the bundle id
```

Patterns are case-insensitive; `*` matches anything, `?` matches one character.
Then `launchctl kickstart -k gui/$(id -u)/local.audiowatch` if it is running as
an agent.

**An allow-listed event is still logged**, with the rule that suppressed it, so
you can always go back and see what was hidden:

```
11:43:30  output-start  Discord Helper  pid 81645  dev[Speakers] (allowed by path /Applications/Discord.app/*)
```

The default list covers Chrome, Safari, Firefox, Arc, Brave, Edge, Spotify,
Music, TV, Podcasts, QuickTime, VLC, IINA, Zoom, Slack, Discord, Teams,
FaceTime, Messages and WhatsApp — matched on bundle id *and* on path, because a
helper process usually has a path and no bundle id. The config file lists them
all, with comments, and is the same text that ships as the defaults.

## How it detects the start of output

Three CoreAudio facts, each measured on this machine (macOS 26.6.2) rather than
taken from documentation:

| API | Sends notifications? |
|---|---|
| `kAudioHardwarePropertyProcessObjectList` | **yes**, within ~1 ms of a process connecting to the HAL |
| `kAudioDevicePropertyDeviceIsRunningSomewhere` | **yes**, when IO starts on a device — but it cannot say which process |
| `kAudioProcessPropertyIsRunningOutput` | **no.** A listener registers successfully (`status 0`) and then never fires |

So no single API answers the question, and the tool combines all three:

1. **The process object list listener is where identity is captured.** A process
   connects to the HAL *before* it makes a sound — measured at 112–220 ms
   before — and it is certainly still alive at that moment, so its pid,
   executable path and bundle id are read and cached right then. This is what
   lets an `output-start` still name a process that has since exited.
2. **`IsRunningOutput` is polled**, every 50 ms, because it does not notify.
   This is what actually detects output starting.
3. **A device reporting IO** drops the poll to 10 ms for 2 seconds, to pin down
   which process was responsible.
4. **Every HAL connect is logged** even if output is never confirmed. A process
   cannot make a sound without connecting first, so this is the backstop: worst
   case you get "`/usr/bin/afplay` connected at 11:43:32.340 and was gone 214 ms
   later", which is still the clue you needed.

Notifications go through `osascript`'s `display notification`, which needs no app
bundle of our own and works from a LaunchAgent. It was verified by posting one
and finding `usernoted` log the presentation 130 ms later. They carry **no
sound**. `audiowatch --test-notify` repeats that check, and tells you to look at
System Settings → Notifications → **Script Editor** if it fails — that is the app
notifications are attributed to.

### What it costs

Measured on this machine: **0.7% of one core and 13 MB resident** at the default
50 ms poll. An idle tick is two property reads per audio process and nothing
else — identity and device lookups happen only when a flag actually moves, which
is what keeps a permanently-resident watcher cheap.

## What still gets past it

Read this before trusting it.

- **A process that holds an output stream open permanently.** Detection is on
  the *transition* of `IsRunningOutput` from 0 to 1. A process that opens an
  output stream at login and keeps it open, writing silence most of the time and
  occasionally writing something audible, never transitions and is never
  reported. On this machine `arkaudiod` (Audio Routing Kit) is exactly this
  shape: permanently running output on MacBook Pro Speakers, both BlackHole
  devices and the LG display. **If the noise arrives through ARK, Loopback,
  BlackHole or JACK, audiowatch will attribute it to the bridge, or not see it
  at all.** These are deliberately *not* on the default allow-list.
- **Sounds played on another process's behalf.** `systemsoundserverd` plays UI
  and alert sounds for other apps, so you will see `systemsoundserverd` and not
  whoever asked. Same for `PowerChime` (the charger sound) and `audiomxd`.
- **Anything that is not a HAL client process at all** — the boot chime, which
  is firmware, and audio generated inside a HAL plug-in driver rather than by a
  client.
- **Which channels.** CoreAudio's per-process API reports the *devices* a
  process is running on, and there is no per-channel attribution in it. So
  audiowatch names the device and `--devices` gives its channel count, but it
  cannot say "channels 3–4 of the Scarlett".
- **A gap shorter than the poll.** A process whose output stream is open for
  less than 50 ms could fall between two polls. In practice the window is far
  longer than the sound, because a process opens the device, plays, and lingers:
  across 13 measured `afplay` runs of 100–500 ms files, the shortest output
  window was **524 ms** — ten times the poll interval. Lower `poll-ms` for more
  margin.
- **`DeviceIsRunningSomewhere` will not fire for a device something else already
  holds.** It is a per-device flag, not a per-process one, so when `arkaudiod`
  already has MacBook Pro Speakers running, a second process starting output on
  that device changes nothing and no notification arrives. On this machine
  several devices are permanently running, so **the poll is the real detector
  and the device listener is only an accelerator** — do not raise `poll-ms` far
  on the assumption that the listener will cover it.
- **A process that exits before its path can be read**, in which case the pid
  and bundle id are still logged and the note says the path was never readable.

If the watcher comes up empty, the escalation is a **process tap**
(`kAudioTapClassID` / `CATapDescription`, in the same headers), which can measure
the actual signal a process produces rather than whether it is running IO. That
would see audio passing through a permanently-open stream, which is the one case
above that matters most.

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

No dependencies. `cargo test` covers the encoding, the filter rules, the log
reader, the CLI, the time parsing and the whole state machine including the
short-lived-process case; the CoreAudio callbacks are covered by hand, as
described above.
