# audiowatch: record

Records channels of this machine's audio devices to 32-bit float WAV files.
Answers name each file by absolute path (and a `resource_link`); they never
carry the audio. Hand the path to another tool to measure or play it.

1. `list_devices` first. Takes use its device names (an unambiguous fragment
   also works).
2. `record` with one or more takes and a length. It waits (at most 50 s) and
   returns finished files.
3. To record while something else happens — make something play, run a render —
   call `record` with `wait: false` (at most 600 s): it returns an `id` and
   the paths the files will have, at once. Do the other thing, then call
   `await_recording(id)`: it returns once the files are finished, exactly as
   a waiting `record` would. The paths are not readable before that.
4. To record in time with a MIDI clock (a drum machine, a DAW sending clock),
   give `bars` instead of `seconds`: every take starts on the clock's next
   downbeat and ends that many bars later, counted in ticks. The answer's
   `clock` says the tempo measured; each take's `sync` says where its first
   frame lies against the downbeat. MIDI clock has no bar line of its own —
   it is counted from the clock's Start — so if the clock was already running
   the take waits for the next Start: ask the person to (re)start the master.
   `beats_per_bar` defaults to 4; `clock` names the MIDI input when more than
   one is sending clock. A waiting call must fit the bars in 50 s at the
   clock's tempo; longer, use `wait: false`.

A take is `DEVICE:in|out:CHANNELS[=PATH.wav]`:

- `in` — what arrives at the device: a mixer's or interface's inputs, a
  microphone, a virtual device's return.
- `out` — what the computer sends to the device. Only devices with **no
  input side** (built-in speakers, displays) can be tapped, on macOS 14.2+.
  A virtual loopback device (BlackHole, Loopback) never needs a tap: what was
  sent to it comes back on its input, so record it `in`. A hardware
  interface's outputs cannot be recorded directly — route the audio to
  BlackHole or Loopback as well and record that.
- `CHANNELS` — one channel (`3`, a mono file) or an adjacent pair (`3-4`,
  stereo), counting from 1.

Several takes record together, each its own file at its device's own rate;
nothing is resampled or mixed. Read `warnings`: each names its take, and says
if audio was dropped (the file has gaps) or none arrived. macOS may answer a
missing permission with silence rather than an error, so measure a first take
before trusting a route.
