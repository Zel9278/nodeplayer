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

## Hosting and latency

VST3 and CLAP plugins run in separate worker processes by default. Each worker owns one
complete plugin instance: DSP, parameters, state and native editor. The graph displays the
latest editor image while native drawing and input run in the worker, so a slow plugin
editor does not hold up graph movement or scaling. JSFX and native plugins keep their
existing hosting.

Each worker adds 4096 frames of audio bridge latency, about 85 ms at 48 kHz or 93 ms at
44.1 kHz, in addition to the plugin's own latency. Delay compensation is enabled with
worker hosting to align parallel graph paths. Plugins in series add their latencies.

Start nodeplayer with `--in-process-plugins` to run VST3/CLAP plugins inside the app instead.
Delay compensation is then off unless `--experimental-pdc` is also present, in either
argument order. `--experimental-plugin-isolation` remains accepted as a compatibility alias
for worker hosting; if both hosting flags are present, the last one selects the mode.
Workers require a supported 64-bit platform and, on Linux, X11/XWayland. A worker setup
failure is reported instead of silently loading the plugin inside the app.
These requirements are checked when creating a plugin. A native Wayland main window can
use separate X11 editor windows when XWayland is available.

## On the node

- **Editor** opens the plugin's own window (VST3 and CLAP), drawn inside the node. **Pop out**
  moves it into a window of its own.
- **Params** opens a list of all parameters with sliders.
- **Bypass** lets the audio through untouched.

VST3/CLAP editors are drawn as images in the graph on macOS, Windows and Linux.
The editor image, node frame, and overlapping UI follow every zoom and pan frame,
including touchpad gestures, without changing the plugin's own UI scale or size.
Scaling uses the latest image immediately, independently of the plugin's drawing rate.
Menus, windows, tooltips and nodes in front of an editor show over it, and clicks there
go to them. A private native host keeps the editor rendering without opening
a second desktop window. Closing the graph editor also destroys its native host.
Clicks and drags are forwarded to the source, and Alt+wheel forwards wheel input.
Pointer events retain Shift, Control and Alt/Option state for modified and fine drags.
On macOS and Windows, double-clicks follow the system's timing settings.
On macOS and Windows, attached or owned popup windows appear over the graph and accept pointer input,
including outside the editor image. Escape or a click outside closes the popup.
Windows layered menus retain their uploaded pixels and transparency in the graph.
Keyboard/IME, drag-and-drop, blocking native menus, and unrelated popup windows are
not forwarded; use **Pop out** for those interactions. Capture can
fail for some GPU-rendered editors. Capture and native input stay in the worker under
default hosting. With `--in-process-plugins`, they run on the app's UI thread.

JSFX nodes show the effect's visible sliders, and effects with graphics (`@gfx`) draw them in
the node, with the mouse, wheel, keys and `gfx_showmenu` menus working. Hidden sliders are under
"Params".

Plugin state is saved with the session and presets, as is whether each editor was open.

When a plugin requests a restart, its instance stays alive while processing pauses for
reactivation. Effects use continuously warmed, latency-aligned dry audio during the
handoff, including with graph delay compensation off. Rapid CLAP restart requests are
coalesced until the control has been quiet for 200 ms, with a maximum wait of one second,
so intermediate knob positions do not each reactivate the plugin. Changing a plugin's
actual lookahead still changes its audio delay. Manual bypass keeps its immediate
behavior when graph delay compensation is off. Wires to pins that no longer exist are
dropped. An isolated worker's latency and parameter metadata updates
preserve the existing audio bridge and its history.
Session autosave waits for isolated state snapshots across UI ticks, so the periodic save
does not pause the interface. Explicit preset saves still request fresh plugin state.

On macOS, popped-out editors use regular primary windows that participate in Stage Manager,
Spaces and window cycling. CLAP host resizes update the parent window before calling the
plugin, keeping autoresized child views and their rendered content at the accepted size.

VST3, CLAP and JSFX receive the player's playback state and song position for audio and MIDI
files. Pause holds the position; Stop returns it to zero. At the end of a file, the position
stays at the end. VST3 and CLAP expose playing/not playing; JSFX also distinguishes paused
from stopped. Processing continues while paused or stopped, including effect tails and live
MIDI. VST3 and CLAP do not advertise a musical tempo, beat position or time signature; JSFX
keeps its default 120 BPM and 4/4.

## When a plugin crashes

Under default hosting, a VST3/CLAP worker that crashes or stops responding is marked as
faulted on its node. **Restart host** starts a new worker from the last saved state and
retained parameter changes. Recovery is explicit; the app does not restart a faulty worker
in a loop.

Plugins hosted inside nodeplayer can take the app down if they crash. nodeplayer notes
which plugin it was calling into; on the next start a banner names it, and it goes on the
disabled list: it isn't loaded, and nodes using it are skipped, until you press "Enable" at
the top of the 🔌 tab.

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
