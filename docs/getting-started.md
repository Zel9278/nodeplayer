# Getting started

## Installing

Download the archive for your system from the
[releases page](https://github.com/Zel9278/nodeplayer/releases) and unpack it anywhere. There
is nothing to install: the archive holds the program, the license and the README.

| System | Archive | Notes |
|---|---|---|
| Linux x86_64 | `nodeplayer-<version>-linux-x86_64.tar.gz` | Needs ALSA, X11 or XWayland, and fontconfig, which desktop systems have. Sound goes through ALSA, so PipeWire and PulseAudio work through their ALSA plugins. |
| Windows x86_64 | `nodeplayer-<version>-windows-x86_64.zip` | Runs on its own (the C++ runtime is built in). Sound goes through WASAPI. |
| macOS (Apple silicon) | `nodeplayer-<version>-macos-arm64.tar.gz` | Not signed: see below. |
| macOS (Intel) | `nodeplayer-<version>-macos-x86_64.tar.gz` | Not signed: see below. |

On macOS, the first start is blocked because the program isn't signed. Allow it in System
Settings → Privacy & Security, or remove the quarantine flag once:

```sh
xattr -d com.apple.quarantine nodeplayer
```

To build from source instead, see [Development](development.md).

## Starting

Start `nodeplayer` (`nodeplayer.exe` on Windows). It can also be given a file or a folder:

```sh
nodeplayer song.flac   # starts with the file loaded
nodeplayer ~/Music     # starts with the folder in the library
```

Dropping files and folders onto the window does the same.

### FFmpeg

nodeplayer decodes the common audio formats itself (FLAC, MP3, WAV, Ogg Vorbis, AAC/M4A,
AIFF, CAF and more). For video files, the formats it can't decode (Opus, AC-3 and others) and
for reading tags, it uses the `ffmpeg` and `ffprobe` programs. It looks for them in its own
folder first, then on the PATH.

On Linux and Windows, when there are none, the first start offers to download a static build
(about 150 MB, the newest release from
[BtbN's FFmpeg-Builds](https://github.com/BtbN/FFmpeg-Builds)) into nodeplayer's data folder.
"Not now" asks again next time, "Don't ask again" stops asking, and while FFmpeg is missing a
button in the status bar offers the download again. On macOS, install FFmpeg yourself, for
example with `brew install ffmpeg`.

Without FFmpeg, the files nodeplayer decodes itself still play.

## The window

![The window](images/screenshot.png)

- **Transport** (top): Open, previous / play-pause / stop / next, repeat and shuffle, the time,
  the playback rate (`1.00×`) and the position bar. The current title and artist are at the
  right.
- **Panels**: Library, Now playing, Presets, Plugins, Misskey, Discord and the Graph are tabs.
  Drag a tab by its title onto the arrows that appear over a panel to put it beside, above or
  below that panel, or onto the middle to stack it there. Dropped anywhere else, it floats as a
  window. Right-click a tab and pick "Close" to close it (any but the graph); the "+" at the end
  of each tab bar opens closed ones again. The layout is kept for the next start.
- **Graph**: where the sound goes. The default graph is Player → Output.
- **Status bar** (bottom): the output device, the latest message (hover it for the earlier
  ones) and "About", which shows the version, the author and the licenses.

## Playing something

1. Press "📁 Add folder…" in the Library, or drop a folder onto the window.
2. Click a track. It plays, and playback moves on to the next one by itself.
3. Space plays and pauses (unless you are typing in a text field).

Everything you set up is saved when nodeplayer closes and every 30 seconds, and comes back at
the next start: the graph and every node's settings, the library, the layout, and the last
track, paused where it was.

Next: [Library and playback](library-and-playback.md).
