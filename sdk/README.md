# nodeplayer-plugin

The SDK for nodeplayer plugins: shared libraries that add DSP nodes to the graph and extend the app (side panel, menu items, reacting to and controlling playback).

## Rust

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
nodeplayer-plugin = { path = "path/to/nodeplayer/sdk" }
```

```rust
use nodeplayer_plugin::{export_plugin, Event, Extension, Host, MenuId, Node, Param, Params, Ui};

struct Gain;

impl Node for Gain {
    const ID: &'static str = "gain";           // stable: saved graphs refer to it
    const NAME: &'static str = "My Gain";
    type Shared = ();                          // data shared with the UI (meters...)

    fn params() -> Vec<Param> {
        vec![Param::new(0, "Gain", -24.0, 24.0, 0.0).unit("dB")]
    }
    fn new(_sample_rate: f32, _max_block: usize) -> Self {
        Gain
    }
    fn process(&mut self, input: [&[f32]; 2], output: [&mut [f32]; 2], params: &Params, _: &()) {
        let g = 10f32.powf(params.get(0) as f32 / 20.0);
        let [l, r] = output;
        for k in 0..l.len() {
            l[k] = input[0][k] * g;
            r[k] = input[1][k] * g;
        }
    }
}

struct NowPlaying {
    host: Host,
    log: MenuId,
}

impl Extension for NowPlaying {
    const PANEL: Option<&'static str> = Some("Now playing");

    fn new(host: Host) -> Self {
        let log = host.add_menu_item("Log the current track");
        Self { host, log }
    }
    fn event(&mut self, e: Event) {
        if e == Event::TrackChanged {
            self.host.log(&format!("{:?}", self.host.track_title()));
        }
    }
    fn panel(&mut self, ui: &mut Ui) {
        ui.label(&self.host.track_title().unwrap_or_default());
        if ui.button("Next") {
            self.host.next();
        }
    }
    fn menu(&mut self, id: MenuId) {
        if id == self.log {
            self.host.log(&format!("{:?}", self.host.track_path()));
        }
    }
}

export_plugin! {
    name: "My plugin",
    version: "0.1.0",
    author: "me",
    nodes: [Gain],
    extension: NowPlaying,
}
```

Build with `cargo build --release` and copy the `.so` into `~/.local/share/nodeplayer/plugins/` (or a folder listed in `$NODEPLAYER_PLUGIN_PATH`). In nodeplayer, "Scan plugins" lists the nodes; extensions start with the app.

`examples/plugin-example` in the nodeplayer repository is a fuller example: a node with its own UI (drawing on a canvas, dragging, the mouse wheel, meters fed from the audio thread through `Shared`) and an extension with a panel, menu items and a timer.

### MIDI

A node gets MIDI pins by setting `const MIDI: u32` to `abi::MIDI_IN`, `abi::MIDI_OUT` or both. `Node::midi` is then called on the audio thread right before `process` in every block, with the block's incoming events (sorted by `time`, the frame within the block) and a `MidiOut` for outgoing ones:

```rust
use nodeplayer_plugin::abi::{MIDI_IN, MIDI_OUT};
use nodeplayer_plugin::{MidiEvent, MidiOut};

impl Node for Octave {
    const MIDI: u32 = MIDI_IN | MIDI_OUT;
    // ...
    fn midi(&mut self, input: &[MidiEvent], out: &mut MidiOut, _: &Params, _: &()) {
        for e in input {
            match e.as_note_on() {
                Some((key, vel)) => { out.push(MidiEvent::note_on(e.time(), e.channel(), key.saturating_add(12), vel)); }
                None => { out.push(*e); }
            }
        }
    }
}
```

Events are short messages (1 to 3 bytes, no sysex). Instruments usually store the events in `midi` and play them at their frame in `process`. In C, set `midi` and `process_midi` in `NpNodeDesc`.

### Threads

- `Node::process` and `Node::midi` run on the audio thread; keep them free of locks, allocation and I/O.
- `Node::ui` and every `Extension` method run on the UI thread, as do all `Host` calls.
- `Params` are atomics, readable from both sides. Setting a value from `Node::ui` takes effect on the next block.
- `Host` commands (play, seek, load a preset...) are carried out after the callback returns; queries (`status`, `track_title`...) describe the current frame.

Panics in plugin code are caught at the boundary and logged to stderr instead of taking nodeplayer down. A real crash (a segfault in C code, say) still ends the process; nodeplayer then disables the plugin at the next start until the user enables it again.

### UI

`Ui` is an immediate-mode interface onto nodeplayer's toolkit: call widgets every frame and they report changes (`button` returns true when clicked, `slider` when moved). Nest layouts with `horizontal`, `vertical`, `group`, `collapsing`, `scroll` and `id_scope` (use the latter around repeated widgets in loops). For custom drawing, `canvas(w, h)` reserves an area and reports pointer interaction with it; `line`, `rect`, `circle` and `text` then draw in UI coordinates (the canvas's `x`/`y` is its top-left corner), clipped to the canvas. Call `request_repaint` while something animates.

## C and C++

`include/nodeplayer_plugin.h` declares the same interface. Export `const NpPluginDesc *nodeplayer_plugin_entry(void)` returning a static description; `examples/c-plugin/gain.c` is a complete example.

```sh
gcc -O2 -shared -fPIC -I path/to/nodeplayer/sdk/include gain.c -o libmy_plugin.so -lm
```

## Versioning

`NpPluginDesc::abi_version` is the version the plugin was built for; this SDK declares 2. nodeplayer loads versions 1 and 2 and refuses others rather than guessing.

- Version 2 added `midi` and `process_midi` at the end of `NpNodeDesc`. Version 1 node tables are read at their shorter stride and get no MIDI.
