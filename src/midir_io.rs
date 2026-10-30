//! MIDI ports through midir (WinMM on Windows, CoreMIDI on macOS), standing in for the ALSA
//! sequencer used on Linux. There is no port of our own here: nodes connect to the system's
//! devices directly, found by name.

use midir::{Ignore, MidiInput, MidiInputConnection, MidiOutput, MidiOutputConnection};

use crate::midi::{Addr, Dir, Port};

const CLIENT: &str = "nodeplayer";

/// Ports a node going in `dir` can connect to: sources for [`Dir::In`], destinations for
/// [`Dir::Out`]. The address is just the place in the list.
pub fn list(dir: Dir) -> Vec<Port> {
    let names: Vec<String> = match dir {
        Dir::In => MidiInput::new(CLIENT).map(|m| m.ports().iter().filter_map(|p| m.port_name(p).ok()).collect()),
        Dir::Out => MidiOutput::new(CLIENT).map(|m| m.ports().iter().filter_map(|p| m.port_name(p).ok()).collect()),
    }
    .unwrap_or_default();
    names
        .into_iter()
        .enumerate()
        .map(|(i, name)| Port { addr: Addr { client: 0, port: i as i32 }, label: name.clone(), name })
        .collect()
}

/// Starts receiving from `port`; `on_message` runs on midir's thread. Stops when the
/// connection is dropped.
pub fn connect_in(port: &Port, mut on_message: impl FnMut(&[u8]) + Send + 'static) -> Result<MidiInputConnection<()>, String> {
    let mut input = MidiInput::new(CLIENT).map_err(|e| e.to_string())?;
    // Like the ALSA side: no sysex, clock or active sensing.
    input.ignore(Ignore::All);
    let ports = input.ports();
    let p = ports.iter().find(|p| input.port_name(p).ok().as_deref() == Some(port.label.as_str())).ok_or("the port is gone")?;
    input.connect(p, "nodeplayer-in", move |_, bytes, _| on_message(bytes), ()).map_err(|e| e.to_string())
}

pub fn connect_out(port: &Port) -> Result<MidiOutputConnection, String> {
    let output = MidiOutput::new(CLIENT).map_err(|e| e.to_string())?;
    let ports = output.ports();
    let p = ports.iter().find(|p| output.port_name(p).ok().as_deref() == Some(port.label.as_str())).ok_or("the port is gone")?;
    output.connect(p, "nodeplayer-out").map_err(|e| e.to_string())
}
