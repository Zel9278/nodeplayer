# Files and troubleshooting

## Where things are kept

| | Linux | Windows | macOS |
|---|---|---|---|
| Settings | `~/.config/nodeplayer` | `%APPDATA%\nodeplayer` | `~/Library/Application Support/nodeplayer` |
| State (play counts, crash notes) | `~/.local/state/nodeplayer` | `%APPDATA%\nodeplayer\state` | `~/Library/Application Support/nodeplayer/state` |
| Cache (tags, lyrics, plugin scans, piano samples) | `~/.cache/nodeplayer` | `%LOCALAPPDATA%\nodeplayer\cache` | `~/Library/Caches/nodeplayer` |
| Data (FFmpeg, native plugins) | `~/.local/share/nodeplayer` | `%LOCALAPPDATA%\nodeplayer` | `~/Library/Application Support/nodeplayer` |

On Linux the `XDG_CONFIG_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME` and `XDG_DATA_HOME`
variables move these.

In the settings folder:

| File | What it holds |
|---|---|
| `session.json` | the graph with every node's settings and plugin states, the library sources, the layout, repeat and shuffle, the playback rate, and the last track and position |
| `presets/<name>.json` | graph presets |
| `loops.json` | each track's loop selection and loop presets |
| `node-presets/<name>.json` | node presets |
| `misskey.json`, `discord.json` | the sharing settings, with their tokens and keys (readable only by you) |
| `setup.json` | the answers to the first-start questions |
| `disabled_plugins.txt` | plugins disabled after a crash |

In the state folder, `plays.json` has the play counts. Everything in the cache folder can be
deleted; it is made again.

## Troubleshooting

### No sound

- Check the status bar: it names the output device, or says "no audio device".
- Check that the Player is wired to an Output, through nodes that aren't bypassed into silence,
  and that the Output's volume is up.
- A MIDI file needs an instrument: wire the Player's MIDI pin to CSynth or an instrument
  plugin (see [MIDI](midi.md#midi-files)).
- nodeplayer uses the system's default output device. On Linux, it goes through ALSA, so with
  PipeWire or PulseAudio pick the device in their mixer.

### A file doesn't play, a video shows no picture, tags are missing

Those need FFmpeg. The status bar offers to download it while it's missing (Linux and
Windows); on macOS, install it, for example with `brew install ffmpeg`. See
[Getting started](getting-started.md#ffmpeg).

### Japanese (or other) text input doesn't work (Linux)

nodeplayer's window is an X11 window, also on Wayland (through XWayland), so the input method
must be reachable through XIM. nodeplayer finds fcitx or ibus by itself when `XMODIFIERS`
isn't set; if it still doesn't work, set it for your session, for example
`XMODIFIERS=@im=fcitx` in `~/.config/environment.d/im.conf`, and log in again.

### Japanese, Chinese or Korean text shows as boxes

nodeplayer adds the system's CJK font (Noto Sans CJK and the like, found through fontconfig).
Install one, for example `google-noto-sans-cjk-fonts` on Fedora or `fonts-noto-cjk` on Debian
and Ubuntu.

### Running as a native Wayland window (Linux)

Plugin editors are X11 windows and can only be embedded into an X11 window, so nodeplayer
runs through XWayland. With `NODEPLAYER_WAYLAND=1` it opens as a Wayland window instead; editors
then open in their own windows.

### The app closed while a plugin was in use

A plugin crashed. At the next start a banner names it and it is disabled; see
[Plugins](plugins.md#when-a-plugin-crashes). Press "Enable" in the 🔌 tab to try it again.

### Clicks land in the wrong place in a Windows plugin (yabridge)

Update yabridge and nodeplayer; nodeplayer works around Wine's misplaced embedded windows. If a
plugin still misbehaves, "Pop out" its editor into its own window.

### Discord shows nothing

Check that the Application ID is set, that nodeplayer says it is connected, and that Discord's
Settings → Activity Privacy → "Share my activity" is on.

### macOS says the app can't be opened

The builds aren't signed. Allow it in System Settings → Privacy & Security, or run
`xattr -d com.apple.quarantine nodeplayer` once.

### Starting over

Quit nodeplayer and move `session.json` away to get the default graph and layout back. To
forget everything, remove the settings, state and cache folders above.
