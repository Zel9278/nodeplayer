# nodeplayer

An audio player written in Rust where the signal chain is built as a node graph. It hosts VST3, CLAP and JSFX plugins plus its own native plugins (DSP nodes and app extensions), and Windows plugins can be used through yabridge. It also plays video files, showing the picture in a Video node.

## Building and running

```sh
cargo run --release              # starts the GUI
cargo run --release -- song.flac # starts with a file loaded
cargo run --release -- ~/Music   # starts with that folder listed
```

You can also drag and drop a file or folder onto the window. Playback uses the default output device through cpal/ALSA, which goes through PipeWire.

Building needs CMake, a C++ compiler, and the fontconfig and freetype development files: the bundled [ysfx](https://github.com/JoepVanlier/ysfx) (in `third_party/ysfx`, for JSFX) is compiled by `build.rs`. FFmpeg isn't needed to build: it's used at run time as the `ffmpeg` and `ffprobe` programs (any recent version), for video, the formats symphonia can't decode, and tags.

### FFmpeg and first start

nodeplayer looks for `ffmpeg` and `ffprobe` in `~/.local/share/nodeplayer/ffmpeg/` first, then on the PATH. When there are none, it asks on start whether to download a static build (about 150 MB, the newest release build from [BtbN's FFmpeg-Builds](https://github.com/BtbN/FFmpeg-Builds), x86_64 or aarch64) into that folder; "Not now" asks again next time, "Don't ask again" stops asking (kept in `~/.config/nodeplayer/setup.json`), and a button in the status bar offers it again while FFmpeg is missing. Without FFmpeg, audio files symphonia reads still play.

### AppImage

```sh
cargo build --release
APPIMAGETOOL=/path/to/appimagetool sh packaging/appimage.sh   # → target/appimage/nodeplayer-x86_64.AppImage
```

Only the program goes in; the window, sound and font libraries are the system's (libX11, libasound, fontconfig, freetype, libstdc++), and FFmpeg comes as above. The AppImage needs a glibc at least as new as the one it was built with. Started from an AppImage, nodeplayer offers once to add itself to the app menu: it copies the AppImage to `~/.local/bin/nodeplayer.AppImage` and writes `~/.local/share/applications/nodeplayer.desktop` and the icon (`assets/icon.svg`, rendered to `assets/icon.png`). Started from another copy later, it offers to replace the installed one. Settings, presets and plugins stay in `~/.config/nodeplayer/` and `~/.local/share/nodeplayer/` as for the plain program.

## How to use it

- **Panels**: the graph, 🎵 Library, 💾 Presets, 🔌 Plugins, 📢 Misskey, 🎮 Discord and (with an extension that draws one) ⚙ Extensions are docked tabs. Drag a tab onto the arrows that appear over a panel to put it beside, above or below that panel, or into the middle to stack it there as a tab; dropped anywhere else it floats as a window inside the main one, and can be dragged back. Right-click a tab and pick "Close" to close it (any but the graph), and the "+" at the end of each tab bar opens closed ones again. The layout is kept with the session. Plugin editors shown inside nodes are hidden while another tab covers the graph.
- **Now playing** (ℹ, under the library): what's playing and how. The bitrate of the second just played (from the sizes of the packets symphonia decodes; files FFmpeg plays show the average), with a graph of the last minute against the file's average (hover it for a value), codec and profile, lossless or lossy, bit depth, the sample rate and whether it's resampled to the output's, channels (and the downmix to stereo), the playback rate, the decoder and the output device. Then the file (folder, container, size, when it was added), its play statistics and every tag. Values can be selected and copied. Layouts saved before the panel existed get it under the library.
- **Library**: "📁 Add folder…" adds folders (with their subfolders) and "🎵 Add files…" adds single tracks; dropping a folder onto the window adds it too. The sources are listed under the buttons (✖ removes one) and kept with the session. The tracks are a table: track number, title, artist, album and length, read from the tags in the background (cached in `~/.cache/nodeplayer/library-tags-2.json`, so only new or changed files are read again). Three more columns can be shown: the release date (the `date` tag), when the file was added (its creation time, else its modification time) and how often it was played. A play counts once half the track, or four minutes of it, has been listened to (seeking doesn't count); the counts are kept in `~/.local/state/nodeplayer/plays.json`. Click a heading to sort by it, click again for descending, a third time for folder order again (with folder headings); playback follows the order shown. Drag a heading's right edge to resize a column; right-click the headings to hide or show columns, or to sort by any column ("Sort by"), shown or not; the title takes the room left. The filter matches file names, folders, titles, artists and albums. Click a track to play it. Playback moves on to the next track automatically. The transport has ⏮ / ⏭, repeat (off / all / one) and shuffle. "⏮" restarts the track if more than 3 seconds have played. Space plays and pauses (except while typing in a text field).
- **Playback rate**: the "1.00×" next to the time is the playback rate shift (0.5–2×): speed and pitch change together, like a tape played faster or slower (the semitones are in its tooltip). Drag it or double-click to type a value; right-click resets it to 1×. A change is heard at once and glides while dragging. The Video and Lyrics nodes follow, and the rate is kept with the session. The same shift is also a node, **Playback Rate Shift** (graph menu), with a slider and bypass: it counts while the Player's audio goes through it (the node says when it doesn't), several in a row multiply, and the transport's rate multiplies them too; the result, capped at 0.5–2×, is shown after the transport's value. The node itself passes audio through, since the rate is applied where the player reads the file. At exactly 1× the audio passes through untouched until the rate is first changed (then it goes through the resampler until the next seek or track).
- **Graph**: right-click an empty area to add a node. Drag between pins to connect them, and right-click a node to remove it. When several wires go into the same input, they are summed. Connections that would create a loop are refused. The graph zooms with Ctrl+wheel; the zoom is shown in its corner, and clicking it goes back to 100%. Text in the graph is only sharp at 100% (the graph is drawn scaled), so a zoom that ends up near 100% snaps to it, and double-clicking to fit the graph doesn't zoom in past it.
- **MIDI**: round blue pins carry stereo audio, square orange pins carry MIDI; MIDI only connects to MIDI. Several MIDI wires into one input are merged in time order. A wire carries up to 16384 events per audio block. Nodes with MIDI pins:
  - MIDI In / MIDI Out (graph menu → MIDI): an ALSA sequencer port (`nodeplayer:MIDI In` / `MIDI Out`). Pick the keyboard or synth in the node, or connect with `aconnect` / qpwgraph; the choice is saved by port name. MIDI Out has a panic button and sends All Notes Off when removed.
  - MIDI Monitor: lists the last events passing through and passes them on.
  - MIDI File Player: plays a standard MIDI file (.mid) out of its MIDI pin: play / pause / stop, a position slider, speed (0.25–4×) and leaving out channel 10. The held keys light up on a keyboard strip. A .mid dropped on the window goes to the first MIDI File Player. The parser and player come from midi-blocks-rs (`crates/midi`): tracks are scanned in parallel and the merged events kept packed (zstd) in memory, so even huge "black MIDI" files load; a seek chases controllers, programs and bends and holds the notes still sounding there. Messages are stamped with when they should sound (20 ms ahead) and land on their frame in the audio block; a note-on that reaches the audio thread more than 100 ms late is left out (counted under the keyboard) instead of making the rest late too.
  - CSynth: [csynth](https://git.c30.life/ced/csynth-v2), the MIDI apps' piano, as an instrument (MIDI in, audio out). It plays the MPP piano samples or a SoundFont (.sf2) picked in the node. The MPP samples are looked for like the apps do (`MPP_WEB_SAMPLES`, next to the executable, `~/.cache/mpp-client-gui`), otherwise downloaded from mpp.c30.life once and cached in `~/.cache/nodeplayer/`. Sounds load in the background and the node keeps playing the old one until the new one is ready. Volume, voice count, short release (the apps' performance mode) and the apps' output limiter are settings of the node. Notes start one 128-frame quantum after they arrive (2.7 ms at 48 kHz) so each keeps its exact frame, and the node adds the sustain pedal (CC 64), which csynth itself doesn't have. It takes black MIDI the way the MIDI apps do: note-ons too quiet to hear are left out, a block plays only the loudest note-ons the voices can take (2048), and a key ends when the last note holding it does (several tracks often strike the same key).
  - Audio → MIDI: its detected notes also come out of its MIDI pin.
  - Plugins that take or send MIDI: VST3 event buses (note on/off and poly pressure as events, CCs, channel pressure and pitch bend through the plugin's `IMidiMapping`), CLAP note ports (MIDI or CLAP note dialect), every JSFX (MIDI bus 0), and native nodes that declare MIDI.
- **Built-in nodes**: Player, Output (master volume and meters), Gain, Pan, EQ, Delay and Mixer (4 inputs).
- **Graphic EQ**: up to 8 bands (peak, low/high shelf, low/high pass at 12–48 dB/oct, band pass, notch) edited on a frequency-response graph: drag a point to move it (frequency / gain, or Q for filters without gain; Shift for fine steps), mouse wheel over a point for Q, double-click empty space to add a band, double-click a point to switch it off, right-click for type, slope and removal. The selected band's exact values are editable below the graph, the output spectrum can be shown behind the curve, and the graph is resizable from its corner. Sessions with the old single-band EQ load as a one-band EQ.
- **Lyrics**: shows the current track's lyrics. Timed (LRC) lyrics follow playback: the current line is highlighted and kept near the middle, clicking a line seeks there, the wheel scrolls by hand (following resumes after 4 s), and "offset" shifts the timing. Plain lyrics just scroll. Font size and the box size (corner handle) are settings. The lyrics come from, first found:
  1. an `.lrc` (or `.txt`) file next to the track with the same name
  2. lyrics embedded in the file's tags
  3. with "online" on: NetEase Cloud Music, then Kugou, searched by title and artist and matched by name and length. These are the services' unofficial web APIs (no key; they may change), and the title, artist and length are sent to music.163.com / kugou.com. The credit lines they put first ("作词 : …") are dropped. Answers are cached in `~/.cache/nodeplayer/lyrics/`; "not found" is asked again after a week, or right away with ⟳.
- **Video**: shows the picture of the file the player is playing (mp4, mkv, webm, mov and anything else FFmpeg reads), in step with the player: pausing, seeking and the next track all follow. The audio still goes through the graph from the Player node; the files symphonia can't decode (most video files: Opus, AC-3 and so on) are decoded with FFmpeg, and a video without sound plays silence for its length. Pictures are decoded on their own thread, scaled down to the chosen height (360p–1080p or original, 720p by default) and turned upright for phone videos. "offset" shifts the picture against the sound. "window" (or double-clicking the picture) also shows it in a window of its own: double-click or F / F11 for full screen, Esc to leave it, Space to pause, ← / → to seek 5 s. The box resizes from its corner.
- **Kazu Limiter**: a port of the `foo_kazulimitter` JSFX (`KazuLimitter.txt`). It's a peak limiter with an instant-attack per-channel peak envelope and an exponential release, stereo-linked by taking the lower gain. The parameters match the original: Threshold, Release, Lookahead, Output and Bypass. The node also draws the gain-reduction meter (0–30 dB) and a gain-reduction history graph, whose speed is set by Graph speed. Notes:
  - "Lookahead" is the decay time of the peak envelope; the signal itself isn't delayed. That's how the original works too.
  - The original plotted the history linearly (1 − gain) against dB labels. Here it is plotted in dB.
- **Audio → MIDI**: a port of `realtime-audio2midi`. Audio passes through unchanged. A separate thread splits the mono mix into 2048-sample frames (hop 1024) and runs the original's three detectors on each frame:
  - level: the auto-gained peak level picks the channel, and the note is 48 + channel
  - FFT peaks: the 10 strongest peaks become notes, with pitch bend (±2 semitones) for the remainder
  - McLeod pitch detection

  Velocity and channel come from the level. A note is released after its note length (default 10 ms) unless it is retriggered.
  - Output goes through the ALSA sequencer port `nodeplayer:Audio2MIDI`. Pick a destination in the node, or connect it with `aconnect` / qpwgraph.
  - "Panic" sends All Notes Off (CC 123) and a pitch-bend reset on every channel. The same happens when the node is removed or the app exits.
  - ⚙ opens the detailed settings, all saved with the session and presets ("Reset to defaults" restores the defaults):
    - Analysis: frame (512–8192), hop (frame ÷ 8 to ÷ 1), input (Mid/L/R)
    - Gain: auto gain on/off and its ceiling, or a fixed manual gain (auto gain is off by default)
    - Velocity:
      - "dB curve" (default) maps loudness in dB onto the velocity range: floor (e.g. −60 dB) is the lowest velocity, 0 dB the highest, and the shape is adjustable. The settings show the curve and the current input level.
      - Loudness is measured as sine-calibrated RMS for the level and pitch detectors, and as each peak's own amplitude for FFT peaks.
      - "Original" uses the original formulas: linear level, the raw FFT magnitude, and the pitch clarity. They mostly saturate at 127.
      - "Original algorithm" sets that mode plus auto gain, i.e. the original behavior.
    - Level notes: base note and MIDI channel
    - FFT peaks: maximum count, threshold, frequency range, MIDI channel, and pitch bend range (sent to the synth as RPN 0 when it changes, when connecting, and on Panic)
    - Pitch detection: McLeod power and clarity thresholds, and MIDI channel
    - Output: transpose, note range, and minimum velocity (Original mode only; in dB-curve mode the floor decides what is dropped)
    - MIDI channel is "by level" (louder notes on higher channels, as in the original, but over the 15 melodic channels: channel 10 is GM's drum channel, where a synth would play every note as a drum, so it's skipped) or a fixed channel 1–16.
  - With the default frame (2048) the FFT bins are about 23 Hz wide at 48 kHz, so low notes from FFT peaks can land a semitone off. Pitch bend corrects for that. This is the same behavior as the original.
- **Visualizers** (right-click → Visualizer): the audio passes through unchanged and is drawn inside the node. Put one inline, or branch it off any output as a dead end. Drag the corner handle to resize it.
  - Spectrum / Spectrogram: the top row has FFT size, channel and (spectrum only) peak, and ⚙ opens more settings in the node. Settings are saved with the session and with presets.
    - FFT size 512–32768 (the menu shows the matching time and Hz-per-bin)
    - Window: Hann / Hamming / Blackman-Harris / Flat top / Rectangular. Levels are normalized so a full-scale sine reads 0 dB with any window.
    - Channel: Mid / Side / Left / Right / L & R (the spectrum draws both as two traces)
    - Frequency range, log/linear axis, and dB range
    - Tilt (dB/oct, pivoting at 1 kHz), and octave smoothing (1/24 to 1/3 oct)
    - Spectrum only: Filled / Line / Bars style, release (how fast the trace falls), and peak hold with its decay speed. Hover to read the frequency and level.
    - Spectrogram only: history length (0.5–60 s) and colors (Heat / Ice / Gray / Rainbow)
  - Oscilloscope: L/R waveforms over 1 to 200 ms, with a zero-crossing trigger.
  - Vectorscope: goniometer, where mono is vertical, plus a correlation meter.
  - Level meter: peak, 300 ms RMS, peak hold and a clip LED. Click it to reset.
- **Plugins** (🔌 tab): press "Scan plugins", then use "+" to add a plugin to the graph. The list shows the format of anything that isn't VST3. On the node, "Editor" opens the plugin's own GUI (VST3 and CLAP), "Params" opens a generic parameter list, and "Bypass" bypasses it. "Add file…" adds a single CLAP, JSFX or native plugin file kept elsewhere (for example a `.txt` JSFX); it's remembered for later scans.
  - VST3: `$VST3_PATH` (colon separated), `~/.vst3`, `/usr/lib/vst3`, `/usr/lib64/vst3`, `/usr/local/lib/vst3`.
  - CLAP: `$CLAP_PATH`, `~/.clap`, `/usr/lib/clap`, `/usr/lib64/clap`, `/usr/local/lib/clap`. The host supports parameters, state, the X11 GUI (embedded like VST3 editors, with zoom through `set_scale` or resizing), timers, file descriptors and latency. When a plugin asks for a restart (new ports or latency), its processor is taken off the audio thread, audio passes through while it's deactivated and activated again, and wires to pins that no longer exist are dropped. VST3 `restartComponent` with I/O, latency, reload or MIDI-mapping changes is handled the same way.
  - JSFX: `$JSFX_PATH`, `~/.config/REAPER/Effects`, `~/.local/share/nodeplayer/jsfx`. Files with a `.jsfx` extension, or extensionless / `.txt` files with a `desc:` line. Imports resolve against the REAPER effects folder. The node shows the visible sliders, and effects with `@gfx` draw their graphics inside the node (mouse, wheel and keys go to the script; resize from the corner). `@gfx` runs on its own thread, so `gfx_showmenu` popups work: the menu appears at the script's position, submenus open on hover, and clicking outside or Escape dismisses it. Slider-only effects show sliders by default; hidden sliders are under "Params".
  - Native: `$NODEPLAYER_PLUGIN_PATH`, `~/.local/share/nodeplayer/plugins`. See [Native plugins](#native-plugins).
  - Binary plugins are scanned in a child process (`nodeplayer --scan-plugin <format> <path>`), so a crashing plugin can't take the app down. Results are cached in `~/.cache/nodeplayer/plugins.tsv`.
  - Loaded plugins run inside nodeplayer, so one that crashes takes the app with it. nodeplayer notes which plugin each thread is calling into; when the process dies from a fatal signal, that plugin file is written to `~/.local/state/nodeplayer/crashed-plugin`. On the next start a banner names it and it goes on the disabled list (`~/.config/nodeplayer/disabled_plugins.txt`): it isn't loaded (nodes using it are skipped, its extension doesn't start) until you press "Enable" at the top of the 🔌 tab.
  - Turning on "Editor" on a node draws the plugin GUI inside that node. "Pop out" moves it into its own window.
  - The graph zoom (Ctrl+wheel, 0.x–2x) is passed on to the plugin GUI, applied once the zoom stops changing for 150 ms:
    - plugins that support `IPlugViewContentScaleSupport` (LSP, among others) redraw their GUI at that scale
    - plugins that are only resizable get a view of the zoomed size (the contents don't scale)
    - plugins that support neither (e.g. OTT via yabridge) stay at their own size. The node still reserves that size, so it doesn't overlap other nodes.

### Saved state and presets

- **Session**: on exit (and every 30 seconds) the app writes `~/.config/nodeplayer/session.json` and restores it at the next start. It contains:
  - the graph: nodes, positions, wiring, every node's settings, plugin state (base64 of the format's own state) and whether each editor was open
  - the folder that was open, repeat / shuffle, and the last track and position. The track comes back paused.
  - The Audio → MIDI destination and the MIDI In / Out ports are saved by port name ("client: port") and reconnected when that port exists.
  - A plugin that can't be found is skipped, along with its wires, and a message is logged.
- **Presets** (💾 tab): saves the whole graph under a name in `~/.config/nodeplayer/presets/<name>.json`. "Load" replaces the current graph. "✖" deletes after a second click, and "Reset graph" goes back to Player → Output after a second click.
- **Node presets**: a few nodes with their wiring and settings, to add again as a group. Select nodes with Shift+drag (or Shift+click), right-click the graph and pick "Save the N selected node(s) as a node preset…". They're added from the graph menu's "Node presets" (at the clicked spot) or with "Add" in the 💾 tab, and kept in `~/.config/nodeplayer/node-presets/<name>.json`. A Player in a node preset stands for the one already in the graph: its wires go to that one.

### Misskey NowPlaying

The 📢 tab posts what's playing to Misskey as a note, like the fooyin plugin `fooyin_misskey_np` (and foobar2000's `foo_misskey_np` before it).

- **When**: when a track starts playing (on by default), when pausing, and when stopping (the ⏹ button, a media-key stop, or the end of the list). The same note isn't posted twice in a row for a new track, and not again within 5 seconds otherwise. "Post now" posts the current track by hand. Posting can be switched off as a whole.
- **Card image**: "Attach a card image" (on by default) adds a picture like media_info's Now Playing card: the cover (embedded, or cover.jpg / folder.png and the like next to the track; a ♪ when there's none), the state (再生中 / 一時停止中 / 停止中, with the time of the pause), title, artist, a progress bar and the album. It's uploaded to the account's Drive, in a "nodeplayer" folder made the first time, and the note links it. When the upload fails (an account logged in before this needs to log in again for Drive access), the note is still posted, without the card, and the tab says why. The preview shows the card too, redrawn as the position and state change.
- **Template fields**: besides the tags and format info (see ⓘ by the template), `%play_count%`, `%first_played%` and `%last_played%` come from the library's play counts and `%added%` from the file's creation time, as foo_playcount names them; a track never played has no `%play_count%`, so `$if(%play_count%,...)` leaves it out. The play count in a note posted as a track starts doesn't include that play yet. Notes that differ only in those are still "the same note".
- **Accounts**: several at once, each with its own visibility (public / home / followers), "local only", and on/off. Type the server (`misskey.io`) and press "Log in": MiAuth opens the browser, and the account is added once nodeplayer is allowed there (it asks for "write:notes", plus "write:drive" and "read:drive" for the card). An access token made on the server works too ("Use an access token instead"). Logging in again to the same account replaces its token.
- **Template**: foobar2000 title formatting: `%field%`, `[...]` (shown only when a field inside has a value), `'literal'`, and `$if`, `$if2`, `$if3`, `$ifequal`, `$ifgreater`, `$stricmp`, `$div`, `$mod`, `$left`, `$right`, `$num`, `$replace`, `$info(...)` and more (the ⓘ next to the template lists them). Missing fields are empty rather than `?`. The fields are read from the file with FFmpeg: tags under foobar2000's names (`%album artist%`, `%tracknumber%` two digits, `%year%` from the date, …) plus `codec`, `codec_profile`, `bitrate` (kbps), `samplerate`, `channels`, `bitspersample` and `encoding` (lossless / lossy), also as `$info(name)` or `%__name%`. The preview shows the note for the current track and its length.
- **Import from fooyin**: when `~/.config/fooyin/misskey_nowplaying.conf` exists, this copies its accounts, template and settings over (accounts already here are kept).
- An `@` in a tag gets a zero-width space after it, so a title can't mention anyone.
- The settings, tokens included, are in `~/.config/nodeplayer/misskey.json`, readable only by you (mode 600). Failed posts show the server's message in the tab and the status bar.

### Discord Rich Presence

The 🎮 tab shows what's playing on Discord as a "Listening to" activity, with a progress bar while it plays (a rate shift makes the bar run faster or slower). Paused, it shows ⏸ and no bar, or nothing if "Keep showing while paused" is off; stopping clears it. It talks to the Discord app on the same machine (also the Flatpak, Snap and Vesktop ones) and connects again by itself when Discord starts later.

- **Application ID**: Discord shows activities under an application. Make one at [discord.com/developers/applications](https://discord.com/developers/applications) (its name, "nodeplayer" for instance, is what "Listening to" says) and paste its Application ID into the tab.
- **Texts**: the first line, second line and cover tooltip are foobar2000 title formatting, like the Misskey template (`%title%`, `%artist%` and `%album%` by default). "Member list shows" picks what follows "Listening to" in the member list: the first line (the song, by default), the second line, or the application's name.
- **Cover**: Discord only shows images from the web.
  - "Upload the track's own cover to Cloudflare R2" puts the embedded picture (or cover.jpg and the like next to the track), as a JPEG of up to 512 px, into an R2 bucket under `nodeplayer/covers/<hash>.jpg` and shows it from the bucket's public address. Each picture goes up once (it's checked for first). It needs the account ID, an R2 API token's access key ID and secret (with write access to the bucket), the bucket, and the bucket's public URL (its r2.dev address or a custom domain).
  - "Otherwise, look the cover up online" (off by default) searches the iTunes Search API for the artist and album, for tracks without a cover of their own or with R2 off. Searches are remembered while the app runs.
- Discord allows a few updates a minute, so a change can take up to 4 seconds to show. The settings, the R2 key included, are in `~/.config/nodeplayer/discord.json`, readable only by you (mode 600).
- Nothing shows up though nodeplayer says it's connected? Check Discord's Settings → Activity Privacy → "Share my activity".

### Media session (MPRIS)

The app registers on D-Bus as `org.mpris.MediaPlayer2.nodeplayer`, so it shows up in the KDE/GNOME media controls and responds to media keys.
- It shows title, artist and album from the tags (or the file name when there are none), plus the length and cover art. Cover art is the embedded picture if there is one, otherwise cover/folder/front.jpg/png next to the track.
- It accepts play/pause/stop, next/previous, seek/set position, open URI, raise and quit.
- Playback control runs in `App::logic`, so moving to the next track and responding to media controls keep working while the window is minimized.

### Japanese text

egui's bundled fonts have no Japanese glyphs. At startup the app asks `fc-match` for the system font for `sans-serif:lang=ja` / `monospace:lang=ja` and adds it as a fallback. On Fedora that's Noto Sans CJK JP, from `google-noto-sans-cjk-fonts`. Korean and Chinese are covered by the same font.

### About the display backend

Plugin GUIs are X11 windows, and an X11 window can only be embedded into an X11 parent. So on a Wayland session the app starts through **XWayland**. Notes:

- With `NODEPLAYER_WAYLAND=1` it runs as a native Wayland window. Editors can't be embedded then and each opens in its own window.
- An embedded editor always sits on top of egui's drawing. Menus, windows and other nodes that overlap it are hidden behind the plugin.

## Windows plugins (yabridge)

1. Install the Windows plugin with Wine. The default destination is something like `~/.wine/drive_c/Program Files/Common Files/VST3`.
2. Install [yabridge](https://github.com/robbert-vdh/yabridge), for example by extracting it to `~/.local/share/yabridge`.
3. In the app's "Windows plugins (yabridge)" panel, use "Add folder…" to register the plugin folder (or "+" on a folder it found), then press "Sync".
4. After a sync the app rescans automatically. The bridged plugins appear in the list from `~/.vst3/yabridge/` with a `win` label.

### Mouse input with Wine 10 and later

With Wine 9.22 and later, Wine doesn't know where an embedded plugin window really is on screen and thinks it sits at the top-left corner, so clicks land in the wrong place (yabridge #382 / #497 / #506). For bridged plugins, nodeplayer sends the window's real screen coordinates to it as a synthetic ConfigureNotify, the same way a window manager would, which works around this (`Editor::announce_position`).

## Native plugins

nodeplayer loads its own plugins: shared libraries built against the `nodeplayer-plugin` SDK in [`sdk/`](sdk/README.md) (Rust) or `sdk/include/nodeplayer_plugin.h` (C/C++). A plugin can provide:

- **DSP nodes**, listed with the other plugins. They get generic parameter sliders, or draw their own UI inside the node, and can have a MIDI input and output (instruments, MIDI effects).
- **One app extension**, started with the app. It can draw a side panel (the ⚙ tab), add items to the ⚙ menu in the transport bar, react to track changes, play/pause and library changes, and control the player: play, pause, seek, next/previous, open files and folders, play library tracks, load and save presets, and set the output volume.

Examples: `examples/plugin-example` (Rust: a bitcrusher, a stereo-width node with its own UI, a MIDI "Transpose" (MIDI in and out), and a toolbox extension with now playing, a sleep timer, play history and a random-track menu item) and `examples/c-plugin/gain.c` (C). Build and install the Rust one with

```sh
cargo build --release -p np-example
mkdir -p ~/.local/share/nodeplayer/plugins
cp target/release/libnp_example.so ~/.local/share/nodeplayer/plugins/
```

Extensions load at startup, so restart nodeplayer after adding one.

## Debug commands

```sh
nodeplayer --scan-plugin <format> <path>             # list the classes in a plugin file (VST3, CLAP, JSFX, Native)
nodeplayer --test-plugin <format> <path> [name]      # instantiate headless, run a sine through it, play a note if it takes MIDI, restart it (VST3/CLAP), round-trip its state
nodeplayer --test-vst3 <bundle> [name]               # the same for VST3 with more detail
nodeplayer --test-editor <bundle> <name> <out.ppm>   # open a VST3 editor for 3 seconds and capture it
cargo test --workspace                               # unit tests, plus decode → resample → graph → output
NODEPLAYER_DEBUG_PLUGIN="[FORMAT:]<path>|<class name>" nodeplayer  # add the plugin at startup and open its editor
```

## Structure

| Path | Role |
|---|---|
| `src/engine/` | Real-time engine. Nodes, schedules and parameters are sent through a lock-free ring, and whatever is replaced is returned to the UI thread to be freed |
| `src/engine/nodes.rs` | Built-in DSP, and `MidiBridge` between the graph's MIDI and other threads |
| `src/midi.rs` | MIDI In / Out (ALSA sequencer) and MIDI Monitor nodes |
| `src/midifile/` | MIDI file parser and player from midi-blocks-rs (indexing, parallel scan, packed events, tempo map, the player thread) |
| `src/midi_file_node.rs` | MIDI File Player node: controls, and placing the player's timed messages on their frames |
| `src/lyrics.rs` | Lyrics node: LRC parsing, local files and tags, NetEase / Kugou lookup and cache, the scrolling view |
| `src/csynth_node.rs` | CSynth instrument node: loading sounds in the background, MIDI to csynth on its clock, sustain pedal |
| `src/player.rs` | Decoder thread (symphonia, or FFmpeg for what it can't open, then rubato) feeding a ring buffer that the source node reads, and the rate shift (a variable-ratio sinc resampler on the audio thread) |
| `src/av.rs` | FFmpeg as programs: `ffprobe` for streams and tags, `ffmpeg` piping audio as f32 and video pictures as RGBA (frame times from `showinfo`), seeking by restarting, rotation |
| `src/setup.rs` | First start: the FFmpeg download and the AppImage's menu entry |
| `src/video.rs` | Video node: the decode thread, following the player's position, the picture in the node and in its own window |
| `src/info.rs` | The Now playing panel |
| `src/discord.rs` | Discord Rich Presence: IPC connection, the activity, covers (R2 upload, iTunes lookup), the tab |
| `src/r2.rs` | Cloudflare R2 through the S3 API: SigV4-signed PUT and HEAD |
| `src/misskey.rs` | Misskey NowPlaying: settings, MiAuth / token login, posting threads, fooyin settings import, the tab |
| `src/card.rs` | The Now Playing card: cover, state, text drawn with ab_glyph, progress bar, PNG |
| `src/titleformat.rs` | The subset of foobar2000 title formatting the note template uses |
| `src/graph.rs` | Graph model and topological sort into a `Schedule` |
| `src/library.rs` | Folder scanning, tag cache, play counts, sorting, next/previous/repeat/shuffle |
| `src/visual/` | Visualizer drawing. The audio thread writes into a lock-free `Tap` (`engine/nodes.rs`), and the UI reads the latest frames from it |
| `src/engine/eq.rs`, `src/eq_editor.rs` | Multi-band EQ: the filters and their response, and the graph editor |
| `src/plugins/` | Format-independent plugin layer (`Instance`, `Editor`, scanning, cache), the X11 window editors are embedded in, and crash attribution (`guard.rs`) |
| `src/vst3/` | VST3 host: module loading, instances, IHostApplication/IMessage/IComponentHandler, editor plus IRunLoop |
| `src/clap/` | CLAP host: entry/factory, host extensions, event queues, processor, editor |
| `src/jsfx/` | JSFX through ysfx: sliders, state, MIDI, `@gfx` on its own thread drawn as a texture, `gfx_showmenu` menus |
| `src/native/` | Native plugins: nodes, extensions and the services they get (`ext.rs`), the UI interface over egui (`ui.rs`) |
| `sdk/` | `nodeplayer-plugin`: the plugin ABI (`abi.rs`, `include/nodeplayer_plugin.h`) and the Rust SDK |
| `src/yabridge.rs` | Wrapper around `yabridgectl` |
| `src/app.rs` | egui / egui-snarl / egui_dock UI |

## Not supported yet

- Sidechain and multi-bus routing, MIDI sysex, transport/tempo sync for plugins (they see a fixed 120 BPM, playing)
- Running plugins in a separate process (a crash is recorded and the plugin disabled, but the app still goes down with it)
- Saving playlists and reading tags (titles and other metadata are not read; file names are shown)
- Latency compensation
