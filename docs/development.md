# Development

## Building

Get the source with its submodules (the bundled [ysfx](https://github.com/JoepVanlier/ysfx),
for JSFX, and what it uses):

```sh
git clone --recursive https://github.com/Zel9278/nodeplayer
cd nodeplayer
cargo run --release              # starts the GUI
cargo run --release -- song.flac # starts with a file loaded
cargo run --release -- ~/Music   # starts with that folder listed
```

`build.rs` compiles ysfx with CMake, so building needs CMake and a C++ compiler besides Rust:

- **Linux**: CMake, a C++ compiler, pkg-config, and the development files of ALSA, fontconfig,
  freetype, X11, xkbcommon and D-Bus. On Debian and Ubuntu:
  `sudo apt install cmake g++ pkg-config libasound2-dev libfontconfig1-dev libfreetype-dev libx11-dev libxkbcommon-dev libdbus-1-dev`.
- **Windows (MSVC)**: install the Visual Studio C++ build tools and CMake, use the
  `x86_64-pc-windows-msvc` Rust toolchain, and run `cargo build` or `cargo run`.
- **Windows (MinGW cross-build)**: release builds are cross-built from Linux: install
  `gcc-mingw-w64-x86-64` and `g++-mingw-w64-x86-64` (the posix-thread flavour, which ysfx needs),
  add the target with `rustup target add x86_64-pc-windows-gnu`, and run
  `cargo build --release --target x86_64-pc-windows-gnu`. The C++ runtime is linked in
  statically, so the `.exe` runs on its own.
- **macOS**: the Xcode command line tools and CMake (`brew install cmake`).

FFmpeg isn't needed to build; it's used at run time as programs. See
[`.github/workflows/build.yml`](../.github/workflows/build.yml) for the exact steps CI takes.

## Formatting and linting

Run these checks before committing Rust changes:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Apply workspace formatting with `cargo fmt --all`. Vendored dependencies and submodules
are not part of this formatting pass. If another process is using the normal Windows debug
executable, use `--target-dir target/isolation` with build, Clippy and test commands instead
of stopping unrelated processes.

VST3/CLAP plugins use whole-plugin worker processes and delay compensation by default.
Each worker owns one instance's DSP, editor, parameters, state and destruction. The bridge
adds 4096 frames per plugin (about 85 ms at 48 kHz), plus the plugin's algorithm latency.
`--in-process-plugins` selects local hosting without PDC; add `--experimental-pdc` to keep
delay compensation in either flag order. `--experimental-plugin-isolation` remains an alias
for worker hosting. When both hosting flags appear, the last one wins. The internal worker
entry and native scan/test commands load locally to avoid recursive worker or scanner
creation; `--test-isolated` explicitly tests the bridge. Unsupported worker setups report
an error rather than loading locally. See the [remote backend](<../src/plugins/remote/README.md>)
for architecture and limitations. Machine-specific plugins, diagnostic scenarios and private
notes can be kept under the ignored `.local/` directory.

Embedded VST3/CLAP editors always use captured presentation on macOS, Windows and Linux,
with default worker hosting or `--in-process-plugins`. The legacy
`--experimental-editor-capture` argument is still accepted by existing launch commands.
Captured editors use a private native rendering host. Graph zoom transforms the latest
image on every frame without waiting for capture, resizing the source, or requesting a
plugin UI-scale change. Popped-out editors use native windows.
Active capture and graph repaint scheduling target 240 Hz. An unchanged editor returns
to the graph's ordinary idle cadence after 250 ms. Pixel, geometry and forwarded input
changes restore fast repainting. Workers capture when the previous image has been read,
or replace an unread image after input, parameter or display-density changes. Native
input and timers continue to run while an image waits for the graph. Actual delivery
depends on the plugin's renderer, native capture cost and event loop; delivered frames
do not measure compositor presentation. Workers poll control readiness without blocking
their native event pump. Windows prints the editor into a client-only DIB and retains
layered popup uploads before their source bitmaps are reused. Captured native popups
stay off the desktop, including position changes made by layered uploads. X11 capture
resolves the current named pixmap on every read so compositor backing-store replacement
cannot leave it reading an abandoned image.
On Windows, captured sources reserve a fixed offscreen origin across resizes.
The source registers capture input before the plugin creates its child windows.
Captured windows retain the nearest display when a monitor query would otherwise
return no display. This keeps JUCE's VBlank redraw listener active offscreen.
Cursor queries and warps inside captured editor callbacks use forwarded coordinates
without moving the desktop cursor. The graph and other threads retain the normal cursor APIs.
Cancellation restores normal cursor and foreground queries until the next pointer event.
Ownerless popup windows created during editor callbacks stay with that editor. Worker
helper callbacks retain the same input context so asynchronous JUCE menus stay open.
Captured popup geometry and hit tests use the menu's logical position while its HWND
stays offscreen, including when a plugin clamps menus to a monitor.
Windows captured editors also forward keyboard focus, key transitions, translated text,
committed host IME text, and clipboard shortcuts. Pointer and keyboard messages share
one ordered queue, with worker generation and editor token checks on delivery.
Focus loss and editor closing release held keys. Translated character messages are
queued before their key-down callback so JUCE does not synthesize the same character a second time.
Plugin focus and key and mouse-button queries use that editor's logical input state
inside immediate and deferred callbacks. Each callback restores the host's keyboard
table afterward, while graph focus and the physical keyboard remain with the host.
Forwarded X11 pointer events obtain timestamps from a private unmapped helper window,
so native click tracking and focus/grab requests use the display's clock.
See [plugins](plugins.md) for input and renderer limitations.

On macOS, each Metal popup uses its own completed-drawable capture adapter.
Capture sources remember weak references to explicitly attached popup windows,
including when AppKit detaches a hidden window that the plugin later reuses.
Local event monitors installed while handling captured editor input leave the
graph's original pointer events to the host while that editor has an interactive popup.
The graph forwards selections and outside-click cancellation in native coordinates.
Forwarded right and middle mouse events preserve their physical button number.
Run the AppKit monitor regression on macOS, from the repository root:

```sh
nodeplayer_input_test=$(mktemp -d)/cocoa-input-test
clang++ -std=c++17 -fobjc-arc -Wall -Wextra -Werror -framework Cocoa \
  src/plugins/surface/cocoa_input_test.mm -o "$nodeplayer_input_test"
"$nodeplayer_input_test"
```

Diagnostic GUI scenarios can wait for `editor-frame` to confirm that an actual captured
image reached the parent renderer. Explicit `graph-zoom` steps accept a `factor` between
0.2 and 3.0, and `screenshot` steps save a PNG named by their `name` in the run artifacts.
Screenshot names contain only letters, digits, hyphens, or underscores.
An optional `audio_file` names a local GUI-test input, resolved relative to the scenario.
It plays through the ordinary player and diagnostic plugin graph; output remains null
unless the scenario explicitly selects device audio. Offline fixtures cannot use it.
The `engine-progress` and `screenshot` events include `editor_frame_stats` with the
delivered frame count, age of the newest delivered frame, elapsed seconds, and average
delivery rate for the current editor open. These count texture staging, not compositor
presentation. Diagnostic capture also counts `changed_frames` by comparing image hashes,
including the initial image. Delivery includes identical images; use the change count
to check that a stationary-cursor animation actually produces new pixels.
For steady delivery measurements, use the periodic progress events before a screenshot,
since PNG encoding pauses the diagnostic UI. The ignored `frame_transfer_timings` test
reports shared-memory publish/read costs separately from capture and rendering.
Progress events also report `ui_frame` and `max_ui_frame_interval_seconds` independently
of editor image delivery. The counter tracks application UI frames, and the interval is
the largest UI frame interval since the previous progress event; neither measures
compositor presentation. This distinguishes slow plugin images from a stalled graph UI.

Captured `open-editor` steps wait for the first delivered image. A worker fault, capture
error, missing startup frame, or stale delivery fails the run even if its later steps
finish. Capture errors remain diagnostic evidence for the current editor open after frame
delivery recovers, even when the visible error clears. The scenario's optional `health`
object sets `capture_startup_timeout_ms`
(default 5000) and `capture_max_frame_age_ms` (default 2000), each between 1 and 60000.
Intentional fixture `fault` steps require the fault to occur and an explicit successful
`restart-host` before passing. Set the fault step's `recover` to `false` only for a
fixture that deliberately ends with its worker faulted. `summary.json` records the
health evidence and the requested outcome separately from the authoritative result.

## Releases

Pushing a `v*` tag builds every platform and publishes a GitHub release with Velopack
installers, portable packages and update feeds. See [Velopack packaging](../packaging/velopack/README.md)
for the outputs and channel names. Windows and Linux build natively; macOS arm64 and
x86_64 compile on Ubuntu with `mbround18/setup-osxcross@v3`. The cross-built test binaries
then run on macOS, where the app bundle is ad-hoc signed and packaged without recompiling.
Branch and PR runs check packaging too. `packaging/macos/make-app.sh` preserves the
document associations and minimum macOS version from `packaging/macos/Info.plist`.

Windows release builds are window apps: they open no terminal. Started from a terminal, they
write their log there. Debug builds keep their console. Only one nodeplayer runs per user
(see `src/instance.rs`); `--new-instance` starts another, for example a development build
while the installed one runs.

## Licenses of what nodeplayer is built from

The About window lists the crates in `Cargo.toml`, every crate in the build and the C/C++ code
built in. The crate list is `assets/licenses.json`, made from `cargo metadata`. A test fails
when it no longer matches `Cargo.lock`; write it again with

```sh
NODEPLAYER_WRITE_LICENSES=1 cargo test about
```

## AppImage

```sh
cargo build --release
APPIMAGETOOL=/path/to/appimagetool sh packaging/appimage.sh   # → target/appimage/nodeplayer-x86_64.AppImage
```

Only the program goes in; the window, sound and font libraries are the system's (libX11, libasound, fontconfig, freetype, libstdc++), and FFmpeg is offered as a download on first start ([Getting started](getting-started.md#ffmpeg)). The AppImage needs a glibc at least as new as the one it was built with. Started from an AppImage, nodeplayer offers once to add itself to the app menu: it copies the AppImage to `~/.local/bin/nodeplayer.AppImage` and writes `~/.local/share/applications/nodeplayer.desktop` and the icon (`assets/icon.svg`, rendered to `assets/icon.png`). Started from another copy later, it offers to replace the installed one. Settings, presets and plugins stay in `~/.config/nodeplayer/` and `~/.local/share/nodeplayer/` as for the plain program.

## Native plugins

nodeplayer loads its own plugins: shared libraries built against the `nodeplayer-plugin` SDK in [`sdk/`](../sdk/README.md) (Rust) or `sdk/include/nodeplayer_plugin.h` (C/C++). A plugin can provide:

- **DSP nodes**, listed with the other plugins. They get generic parameter sliders, or draw their own UI inside the node, and can have a MIDI input and output (instruments, MIDI effects).
- **One app extension**, started with the app. It can draw a side panel (the ⚙ tab), add items to the ⚙ menu in the transport bar, react to track changes, play/pause and library changes, and control the player: play, pause, seek, next/previous, open files and folders, play library tracks, load and save presets, and set the output volume.

Examples: `examples/plugin-example` (Rust: a bitcrusher, a stereo-width node with its own UI, a MIDI "Transpose" (MIDI in and out), and a toolbox extension with now playing, a sleep timer, play history and a random-track menu item) and `examples/c-plugin/gain.c` (C). Build and install the Rust one with

```sh
cargo build --release -p np-example
mkdir -p ~/.local/share/nodeplayer/plugins
cp target/release/libnp_example.so ~/.local/share/nodeplayer/plugins/
```

On Windows the library is `np_example.dll` and on macOS `libnp_example.dylib`; the plugins folder is in the data folder listed in [Files and troubleshooting](files-and-troubleshooting.md).

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

## Source layout

| Path | Role |
|---|---|
| `src/engine/` | Real-time engine. Nodes, schedules and parameters are sent through a lock-free ring, and whatever is replaced is returned to the UI thread to be freed |
| `src/engine/nodes.rs` | Built-in DSP, and `MidiBridge` between the graph's MIDI and other threads |
| `src/midi.rs` | MIDI In / Out (ALSA sequencer) and MIDI Monitor nodes |
| `src/midifile/` | MIDI file parser and player from midi-blocks-rs (indexing, parallel scan, packed events, tempo map, the player thread) |
| `src/midi_song.rs` | MIDI files as songs: the Player node's MIDI section, and placing the player's timed messages on their frames |
| `src/lyrics.rs` | Lyrics panel: LRC parsing, the providers (local files and tags, NetEase / Kugou / LRCLIB lookup and cache), each track's provider and offset, the scrolling view |
| `src/csynth_node.rs` | CSynth instrument node: loading sounds in the background, MIDI to csynth on its clock, sustain pedal |
| `src/about.rs` | The About window: author, license, and the crate list (`assets/licenses.json`) with the test that makes it |
| `src/player.rs` | Decoder thread (symphonia, or FFmpeg for what it can't open, then rubato) feeding a ring buffer that the source node reads, and the rate shift (a variable-ratio sinc resampler on the audio thread); MIDI files go to `midi_song.rs` instead |
| `src/av.rs` | FFmpeg as programs: `ffprobe` for streams and tags, `ffmpeg` piping audio as f32 and video pictures as RGBA (frame times from `showinfo`), seeking by restarting, rotation |
| `src/setup.rs` | First start: the FFmpeg download and the AppImage's menu entry |
| `src/video.rs` | Video panel: the decode thread, following the player's position, the picture in the panel and in its own window |
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
