# Native editor graph-zoom proof of concept

This isolated program tests displaying a native editor as an image in an egui graph.
The editor image, node frame, and overlapping node share one graph transform. Zooming and
panning do not resize the native editor or call a plugin's GUI scaling API. Pointer
coordinates are converted through the inverse graph transform before native delivery.

The native source remains visible in a separate reference window. Without a plugin, it is
an actual platform-native test surface with a counter and slider. With `--plugin`, a CLAP
editor is attached to that native window. This experiment does not alter nodeplayer's
production editor handling, play audio, or activate plugin DSP.

Run from the repository root:

```sh
cargo run --manifest-path examples/editor-zoom-poc/Cargo.toml
```

The graph supports Ctrl+wheel or a pinch to zoom, dragging its background to pan, and
dragging the editor node's title to move it. The 50%, 100%, and 150% buttons zoom about the
view center. Click and drag inside the editor image to forward pointer input. Alt+wheel
forwards the wheel to the native editor; an ordinary wheel pans the graph.

To open a real CLAP editor, pass a native plugin file or macOS bundle:

```sh
cargo run --manifest-path examples/editor-zoom-poc/Cargo.toml -- \
  --plugin /path/to/Plugin.clap
```

`--id <text>` selects a plugin by a case-insensitive name or ID substring when a file
contains multiple plugins. Plugins must support an embedded GUI for the native platform.

## Native backends

| Platform | Capture and pointer path |
| --- | --- |
| macOS | AppKit view bitmap capture, including conversion from Retina pixels to native view coordinates. Pointer events are delivered to hit-tested descendants. Standard AppKit buttons use release-time clicks to avoid entering a blocking tracking loop. |
| Windows | Client-area `PrintWindow` into a top-down DIB, explicit `WM_PRINT` child painting, and a client-DC fallback. Mouse messages go to the appropriate child HWND; native mouse capture is released so the graph retains its input stream. |
| Linux | XComposite redirects a mapped native X11 window hierarchy to a named pixmap. The program reads that pixmap and routes synthetic pointer events into native descendants. Requires XComposite and a TrueColor visual. Wayland sessions use XWayland; a native Wayland plugin backend is not implemented. |

Capture currently reads back to the CPU and uploads an RGBA image, capped at roughly 30
updates per second. The graph can repaint and change its transform independently. This proves the
coordinate and presentation approach; it does not establish a production latency or
bandwidth budget.

Capture compatibility depends on the editor renderer. AppKit bitmap capture and
`PrintWindow` are not universal capture solutions for arbitrary Metal/OpenGL/Direct3D
plugin content. The Windows client-DC fallback also depends on the source window's visible
contents. A successful native fixture test does not establish compatibility with every
third-party plugin.

Pointer forwarding is experimental. Keyboard and IME input, accessibility, drag-and-drop,
and plugin popup windows outside the captured hierarchy are not bridged. Editors that run
their own blocking mouse-tracking loop or require real OS capture may need a different
input path. The CLAP host supports GUI callbacks, timers, and Linux POSIX file-descriptor
notifications, but no audio processing, parameter-flush host extension, or DSP restarts.
Plugin shared libraries remain mapped until process exit so that native GUI objects can
finish destruction safely; GUI/plugin objects are destroyed and the CLAP module is
deinitialized normally.

## Verified coverage

Native GUI runs completed at 50%, 100%, and 150% graph zoom on all three platforms.
The source editor and capture dimensions stayed fixed, the fixture counter advanced on
each mapped click, and every final GUI smoke returned successfully after teardown.

- macOS: native fixture, JuicyMaximus, and Corpus CLAP editors, including Retina capture.
  A separate mapped click also changed JuicyMaximus's Solo control.
- Windows: native fixture on an interactive console desktop, plus capture and click
  routing through an offset child HWND. The installed CLAP files exposed no GUI, so
  third-party Windows editor capture remains unverified.
- Linux: native fixture and LSP Parametric Equalizer x16 Stereo 1.2.37 under Debian 12,
  Xvfb, XComposite, and Mesa lavapipe. Backend tests also covered obscuration, resizing,
  descendant capture, and drag routing. Physical GPU drivers, desktop compositors, and
  native Wayland were not covered by these runs.

Vendor GUI smoke checks rendering; fixture checks assert native pointer delivery.
Arbitrary vendor controls and wheel handling are not comprehensively validated.

## Reproducible checks

These commands create their output directories. Keep generated images and reports in
`artifacts/` (ignored by Git) or outside the checkout.

```sh
cargo test --manifest-path examples/editor-zoom-poc/Cargo.toml

# Native window capture, nonuniform/opaque pixel checks, and a forwarded counter click.
cargo run --manifest-path examples/editor-zoom-poc/Cargo.toml -- \
  --backend-smoke examples/editor-zoom-poc/artifacts/native

# Actual egui/wgpu screenshots at 50%, 100%, and 150% graph zoom.
cargo run --manifest-path examples/editor-zoom-poc/Cargo.toml -- \
  --smoke examples/editor-zoom-poc/artifacts/graph
```

The GUI smoke exits automatically. It marks success only after application teardown has
returned. Its JSON report records the image rectangle, source
dimensions, inverse pointer-mapping error, native counter value, and capture count. Native
counter clicks are injected through the same coordinate mapping used by the viewer; they
are not a test of physical OS mouse injection. Screenshots show the editor image and an
overlapping graph node in the actual renderer.

Either smoke command accepts `--plugin` and `--id`. Vendor-plugin smoke captures the real
GUI without injecting arbitrary parameter changes. The fixture smoke checks pointer
delivery through a known control.

On Linux, the display-dependent backend tests can also run under an existing X11 server
or a disposable Xvfb display:

```sh
xvfb-run -a cargo test --manifest-path examples/editor-zoom-poc/Cargo.toml -- --ignored
```

The program has its own Cargo workspace and lockfile so these checks do not build
nodeplayer's audio engine or require changing its dependencies.

On Windows, run native capture checks in a logged-in interactive desktop session. The
Windows OpenSSH service's noninteractive desktop can return blank captures even when the
same executable succeeds in the console session. The Windows backend also has an ignored
desktop-dependent child-window capture/input test, runnable with `cargo test -- --ignored`
from this package directory.
