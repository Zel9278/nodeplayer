# nodeplayer

A music player where the sound goes through a node graph you build yourself. Tracks come out of
a Player node, run through effects, plugins and visualizers, and play from an Output node.
Written in Rust, for Linux, Windows and macOS.

![nodeplayer](docs/images/screenshot.png)

## Features

- **Library**: folders and files with their tags, sorting, filtering, play counts, repeat and
  shuffle; audio, video and MIDI files are all tracks
- **Node graph**: EQ with a graphic editor, limiter, delay, mixer, gain and pan, and
  visualizers (spectrum, band analyzer, spectrogram, oscilloscope, vectorscope, level meter);
  groups (a graph inside a node); graph and node presets
- **Plugins**: VST3, CLAP and JSFX, with their editors drawn inside the nodes; Windows plugins
  on Linux through yabridge; its own plugin SDK for DSP nodes and app extensions
- **Playback rate**: 0.5–2×, speed and pitch together like a tape, also for MIDI files
- **Selection loop**: a stretch of a track played over and over without a gap, chosen in an
  editor that draws the waveform like an oscilloscope, down to the samples
- **MIDI**: MIDI files (even black MIDI) played through the built-in piano CSynth or any
  instrument plugin, MIDI ports, a MIDI monitor, and audio-to-MIDI conversion
- **Lyrics and video**: synced lyrics (local LRC files, tags, or online) and the picture of
  video files, in step with the player
- **Now playing**: a detail panel with a live bitrate graph, posts to Misskey, Discord Rich
  Presence, and the system's media controls and keys

## Download

Builds for Linux (x86_64), Windows (x86_64) and macOS (Apple silicon and Intel) are on the
[releases page](https://github.com/Zel9278/nodeplayer/releases). Unpack and run; FFmpeg, used
for video and some formats, is offered as a download on first start (on macOS, install it with
`brew install ffmpeg`). The macOS builds aren't signed: allow them in System Settings → Privacy
& Security, or run `xattr -d com.apple.quarantine nodeplayer` once.

## Documentation

The [guide](docs/README.md) explains everything in detail:

1. [Getting started](docs/getting-started.md)
2. [Library and playback](docs/library-and-playback.md)
3. [The node graph](docs/node-graph.md)
4. [MIDI](docs/midi.md)
5. [Plugins](docs/plugins.md)
6. [Lyrics and video](docs/lyrics-and-video.md)
7. [Sharing what's playing](docs/sharing.md)
8. [Files and troubleshooting](docs/files-and-troubleshooting.md)
9. [Development](docs/development.md): building, the AppImage, native plugins, debug commands,
   the source layout

## Building

```sh
git clone --recursive https://github.com/Zel9278/nodeplayer
cd nodeplayer
cargo run --release
```

Building needs CMake and a C++ compiler for the bundled ysfx, and on Linux the ALSA,
fontconfig, freetype, X11, xkbcommon and D-Bus development files. See
[Development](docs/development.md) for each platform.

## Not supported yet

- Sidechain and multi-bus routing, MIDI sysex, transport and tempo sync for plugins (they see a
  fixed 120 BPM, playing)
- Running plugins in a separate process (a crash is recorded and the plugin disabled, but the
  app still goes down with it)
- Saving playlists
- Latency compensation

## License

nodeplayer is made by Zel9278 and released under the MIT License (`LICENSE`). "About" at the right of the status bar shows it, the crates named in `Cargo.toml` with their versions and licenses, the C/C++ code built in (ysfx and what it uses, under Apache-2.0, Zlib and public-domain-or-MIT terms) and the copy of winit changed for IME input (Apache-2.0; the change is marked in `third_party/winit/src/platform_impl/linux/x11/event_processor.rs`), and every crate in the build. That list is `assets/licenses.json`, made from `cargo metadata`; a test fails when it no longer matches `Cargo.lock`, and `NODEPLAYER_WRITE_LICENSES=1 cargo test about` writes it again. Of the crates, symphonia is MPL-2.0 and the rest are MIT, Apache-2.0 or similar permissive licenses. FFmpeg is not part of nodeplayer: it runs as its own programs, under the license of the build installed.
