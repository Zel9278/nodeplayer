# MIDI

## MIDI files

MIDI files (`.mid`, `.midi`) are songs like any other: they are listed in the library, opened
with 📂 Open or by dropping them on the window, and follow the transport: play and pause,
seeking, ⏮ / ⏭, and the next track at the end.

A MIDI file has no sound of its own. The Player node sends it out of its **MIDI** pin (the
orange one under `out`), so connect that pin to an instrument:

1. Right-click the graph → MIDI → **CSynth** (or add an instrument plugin).
2. Wire the Player's `MIDI` pin to CSynth's `MIDI` input.
3. Wire CSynth's `out` to the Output (or into your effects).
4. Play a MIDI file.

While a MIDI file is loaded, the Player node shows its tracks and notes, the held keys on a
keyboard strip, how many notes have played, and **no drums**, which leaves out channel 10.

Even very large "black MIDI" files load: the tracks are read in parallel and kept packed in
memory. Seeking restores the controllers, programs and bends of that place and the notes still
sounding there. A note that would sound more than 100 ms late (a file denser than the machine
can play) is left out rather than delaying the rest, and is counted under the keyboard. The
library shows the length of files up to 16 MB.

### The playback rate

The [playback rate](library-and-playback.md#the-playback-rate) changes a MIDI file's speed
and pitch together, as it does audio:

- each channel's notes move by whole semitones, and the rest of the shift goes on the
  channel's pitch bend, on top of the file's own bends and over the bend range the file sets
  (2 semitones until it sets one), so the pitch glides while the rate is dragged
- a channel only changes its whole semitones while it holds no notes and its sustain pedal is
  up, so sounding notes stay in tune and end on the key they started on
- channel 10 (drums) is left as it is, since its keys are different drums
- at 1× everything passes through untouched

An instrument that ignores pitch bend, or its range setting, gets only the whole semitones.

## CSynth

CSynth is an instrument (the MPP apps' piano): MIDI in, audio out. It plays the MPP piano
samples, or SoundFonts (`.sf2`) and SFZ instruments (`.sfz`, with WAV, FLAC or Ogg Vorbis
samples) added in the node.

- **Several sounds**: "Add SoundFont or SFZ…" adds one or more files to the list. Each
  program is played by the first file in the list that has it (else by the first with the
  General MIDI program it falls back to), so put a piano SFZ above a full General MIDI
  SoundFont to play the piano part with it and the rest with the SoundFont. An SFZ instrument
  counts as the piano (program 1); alone, it plays every channel.
- ⏶ / ⏷ move a file up or down the list, ✖ removes it, and "MPP piano" clears the list. A file
  that can't be loaded is left out (the node says why) and the rest still play.
- The MPP samples are downloaded once from mpp.c30.life and cached, unless they are found
  locally (`MPP_WEB_SAMPLES`, next to the program, or the MPP apps' cache).
- A new sound loads in the background, and the old one keeps playing until it is ready.
- Settings: volume, voice count (up to 2048), short release (performance mode) and the output
  limiter.
- It handles the sustain pedal (CC 64) and takes black MIDI: note-ons too quiet to hear are
  left out, and a block plays only the loudest notes the voices can take.

## MIDI ports

- **MIDI In** brings a keyboard or another program into the graph; **MIDI Out** sends the
  graph's MIDI to a synth or another program. Pick the port in the node; the choice is saved by
  port name and reconnected when the port comes back.
  - On Linux these are ALSA sequencer ports (`nodeplayer:MIDI In`, `nodeplayer:MIDI Out`), so
    they can also be connected with `aconnect` or qpwgraph.
  - On Windows and macOS they connect to the system's MIDI ports (WinMM, CoreMIDI).
  - MIDI Out has a panic button, and sends All Notes Off when it is removed.
- **MIDI Monitor** lists the last events going through it and passes them on. Put it between
  two nodes to see what is sent.

Several MIDI wires into one input are merged in time order.

## Audio → MIDI

A port of `realtime-audio2midi`: the audio passes through unchanged, and the notes heard in it
come out of its MIDI pin and a MIDI port (`nodeplayer:Audio2MIDI` on Linux). It uses three
detectors: the level, the strongest FFT peaks (with pitch bend for the remainder) and McLeod
pitch detection.

- "Panic" sends All Notes Off and resets the pitch bend on every channel.
- ⚙ opens the settings, saved with the session and presets ("Reset to defaults" restores
  them; "Original algorithm" sets up the original's behaviour):
  - analysis: frame size (512–8192), hop, and input (Mid, L, R)
  - gain: automatic with a ceiling, or fixed
  - velocity: a dB curve (a floor such as −60 dB is the lowest velocity, 0 dB the highest; the
    settings show the curve and the input level) or the original formulas
  - per detector: base note, threshold, frequency range, MIDI channel and pitch bend range
  - output: transpose, note range, minimum velocity
  - channels: "by level" (louder notes on higher channels, skipping channel 10) or a fixed one
- With the default frame (2048 samples) FFT bins are about 23 Hz wide at 48 kHz, so low notes
  can land a semitone off; the pitch bend corrects it.

## Plugins with MIDI

VST3 instruments and effects with event buses, CLAP plugins with note ports, every JSFX
(MIDI bus 0), and native plugins that declare MIDI get MIDI pins. See [Plugins](plugins.md).

Next: [Plugins](plugins.md).
