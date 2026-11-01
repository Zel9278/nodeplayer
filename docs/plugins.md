# Plugins

nodeplayer hosts VST3, CLAP and JSFX plugins and its own native plugins. On Linux, Windows
VST3 plugins can be used through yabridge.

## Adding a plugin

1. Open the 🔌 Plugins tab and press **Scan plugins**.
2. Find the plugin (type in the filter) and press **+** to add it to the graph.
3. Wire it between the Player and the Output.

The list marks the format of anything that isn't VST3. "Add file…" adds a single CLAP, JSFX
or native plugin kept elsewhere, and "Add bundle…" a `.vst3` or `.clap` folder; both are
remembered for later scans.

## Where plugins are looked for

| Format | Linux | Windows | macOS |
|---|---|---|---|
| VST3 | `$VST3_PATH`, `~/.vst3`, `/usr/lib/vst3`, `/usr/lib64/vst3`, `/usr/local/lib/vst3` | `$VST3_PATH`, `%LOCALAPPDATA%\Programs\Common\VST3`, `C:\Program Files\Common Files\VST3` | `$VST3_PATH`, `~/Library/Audio/Plug-Ins/VST3`, `/Library/Audio/Plug-Ins/VST3` |
| CLAP | `$CLAP_PATH`, `~/.clap`, `/usr/lib/clap`, `/usr/lib64/clap`, `/usr/local/lib/clap` | `$CLAP_PATH`, `%LOCALAPPDATA%\Programs\Common\CLAP`, `C:\Program Files\Common Files\CLAP` | `$CLAP_PATH`, `~/Library/Audio/Plug-Ins/CLAP`, `/Library/Audio/Plug-Ins/CLAP` |
| JSFX | `$JSFX_PATH`, `~/.config/REAPER/Effects`, `~/.local/share/nodeplayer/jsfx` | `$JSFX_PATH`, or "Add file…" | `$JSFX_PATH`, or "Add file…" |
| Native | `$NODEPLAYER_PLUGIN_PATH`, the `plugins` folder in nodeplayer's data folder | same | same |

The data folder is listed in [Files and troubleshooting](files-and-troubleshooting.md).

## On the node

- **Editor** opens the plugin's own window (VST3 and CLAP), drawn inside the node. **Pop out**
  moves it into a window of its own.
- **Params** opens a list of all parameters with sliders.
- **Bypass** lets the audio through untouched.

The graph zoom is passed on to editors that can scale (LSP plugins, among others); editors
that can only be resized get the zoomed size, and the rest keep their own size. Menus,
windows, tooltips and nodes in front of an editor show over it, and clicks there go to them,
not to the plugin.

`--experimental-editor-capture` displays VST3/CLAP editors as images in the graph.
The editor image, frame, and overlapping UI zoom and move together without changing the
plugin's own UI scale. The source editor remains in a separate visible window; closing it
also closes its graph image. Clicks and drags are forwarded to the source, and Alt+wheel
forwards wheel input. Keyboard/IME, drag-and-drop, and separate native popup windows are
not forwarded; use the source window or **Pop out** for those interactions. Capture can
fail for some GPU-rendered editors. The flag also works with
`--experimental-plugin-isolation`, where capture and native input stay in the worker.
On macOS, isolation requires this captured-editor mode. Linux requires X11/XWayland.

JSFX nodes show the effect's visible sliders, and effects with graphics (`@gfx`) draw them in
the node, with the mouse, wheel, keys and `gfx_showmenu` menus working. Hidden sliders are under
"Params".

Plugin state is saved with the session and presets, as is whether each editor was open.

When a plugin changes its ports or latency (some do when a setting changes), it is restarted:
the audio passes through while it is, and wires to pins that no longer exist are dropped.

## When a plugin crashes

Plugins run inside nodeplayer, so one that crashes takes the app with it. nodeplayer notes
which plugin it was calling into; on the next start a banner names it, and it goes on the
disabled list: it isn't loaded, and nodes using it are skipped, until you press "Enable" at the
top of the 🔌 tab.

Scanning is safe: each plugin file is scanned in a separate process, so a plugin that crashes
while being scanned doesn't take the app down.

## Windows plugins on Linux (yabridge)

1. Install the Windows plugin with Wine. VST3 plugins usually go to
   `~/.wine/drive_c/Program Files/Common Files/VST3`.
2. Install [yabridge](https://github.com/robbert-vdh/yabridge), for example by unpacking it
   into `~/.local/share/yabridge`.
3. In the "Windows plugins (yabridge)" section of the 🔌 tab, "Add folder…" registers the
   plugin folder (or press "+" on a folder it found), then press **Sync**.
4. After a sync the plugins are scanned again; the bridged ones are listed from
   `~/.vst3/yabridge/` with a `win` label.

With Wine 9.22 and later, Wine misplaces embedded windows so clicks land in the wrong spot;
nodeplayer tells the plugin window where it really is, which works around it.

## Native plugins

Native plugins are shared libraries made with nodeplayer's plugin SDK. A plugin can provide
DSP nodes (with their own UI drawn in the node, and MIDI in and out) and one app extension (a
side panel, items in the ⚙ menu, and control of the player). Put the library in the plugins
folder, or point `NODEPLAYER_PLUGIN_PATH` at its folder; extensions load at start, so restart
nodeplayer after adding one. Writing one is described in [Development](development.md#native-plugins).

Next: [Lyrics and video](lyrics-and-video.md).
