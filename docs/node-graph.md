# The node graph

The Graph tab is where the sound goes. Every track comes out of the **Player** node; what
reaches an **Output** node is what you hear.

## Working with nodes

- **Add**: right-click an empty spot and pick a node from the menu. Plugins are added from
  the 🔌 Plugins tab with "+".
- **Connect**: drag from an output pin (right side) to an input pin (left side). Several wires
  into one input are summed. A wire that would make a loop is refused.
- **Pins**: round blue pins carry stereo audio, square orange pins carry MIDI. MIDI only
  connects to MIDI.
- **Disconnect**: right-click a wire to cut it, or right-click a pin to drop all its wires.
- **Remove**: right-click a node and pick "Remove".
- **Fold**: ▼ at the left of a node's title hides its body.
- **Select several**: Shift+drag a box, or Shift+click nodes.
- **Zoom**: Ctrl+wheel. The zoom is shown in the corner; click it to go back to 100%. Text is
  only sharp at 100%, so a zoom that ends near 100% snaps to it. Double-click empty space to
  fit the graph.

Nodes that aren't connected to anything still run, so a visualizer can hang off any output
as a dead end.

## Built-in nodes

| Node | What it does |
|---|---|
| Player | The current track: audio out, and MIDI out when the track is a MIDI file. There is one. |
| Output | Plays what comes in, with the master volume and level meters. |
| Gain | Volume in dB. |
| Pan | Left–right balance. |
| EQ | Up to 8 bands, edited on a graph (below). |
| Delay | Echo with time, feedback and mix. |
| Mixer | Four inputs, each with its own gain. |
| Kazu Limiter | Peak limiter (below). |
| Playback Rate Shift | See [the playback rate](library-and-playback.md#the-playback-rate). |
| Lyrics, Video | See [Lyrics and video](lyrics-and-video.md). |
| MIDI In, MIDI Out, MIDI Monitor, CSynth, Audio → MIDI | See [MIDI](midi.md). |

### EQ

Up to 8 bands: peak, low and high shelf, low and high pass (12–48 dB/oct), band pass and
notch.

- Drag a point to move it: frequency and gain, or Q for filters without gain. Hold Shift for
  fine steps.
- The mouse wheel over a point changes its Q.
- Double-click empty space to add a band; double-click a point to switch it off.
- Right-click a point for its type, slope and removal.
- The selected band's exact values can be typed below the graph.
- "Analyzer" shows the output spectrum behind the curve. The graph resizes from its corner.

### Kazu Limiter

A port of the `foo_kazulimitter` JSFX: a peak limiter with an instant attack and an
exponential release, stereo-linked. Threshold, Release, Lookahead, Output and Bypass match the
original. The node draws the gain reduction (0–30 dB) and its history; "graph speed" sets how
fast the history scrolls. As in the original, "Lookahead" is the decay time of the peak
envelope, and the signal itself isn't delayed.

## Visualizers

Right-click → Visualizer. The audio passes through unchanged and is drawn inside the node;
drag the corner handle to resize it. Put one inline, or branch it off any output.

- **Spectrum** and **Spectrogram**: the top row has the FFT size, the channel and (spectrum
  only) peak hold; ⚙ opens the rest:
  - FFT size 512–32768 (the menu shows the matching time and Hz per bin)
  - window: Hann, Hamming, Blackman-Harris, Flat top or Rectangular (a full-scale sine reads
    0 dB with any of them)
  - channel: Mid, Side, Left, Right, or L & R (two traces on the spectrum)
  - frequency range, log or linear axis, dB range
  - tilt (dB per octave around 1 kHz) and octave smoothing (1/24 to 1/3 octave)
  - spectrum: Filled, Line or Bars, release speed, peak hold and its decay; hover to read a
    frequency and level
  - spectrogram: history length (0.5–60 s) and colours (Heat, Ice, Gray, Rainbow)
- **Band analyzer**: the spectrum in bands, as bars. The top row picks how the bands are
  measured, how they are drawn, and peak caps; ⚙ opens the rest:
  - measuring: **Constant-Q** (each band analyzed with a window fitting its width, so the bass
    is sharp and the treble quick), **FFT** (one FFT for all bands, 512–32768 points),
    **SWIFT** (sliding resonators updated on every sample) or **Analog filters** (band-pass
    filters with an RMS detector, like a hardware analyzer). For the last three, "Band width"
    sets how wide each band listens, and "Order" how many filters each band has in a row
    (more keeps neighboring bands apart); the analog filters also have a detector time. A
    full-scale sine reads 0 dB in its band with any of them.
  - bands: octave bands centered on the notes (1 to 48 per octave, tuned to A4 = 400–480 Hz;
    12 gives one band per semitone), or 4–512 bands on a logarithmic, linear, Bark, ERB or Mel
    scale
  - looks: Bars, LED bars, Curve, Radial bars and Radial curve
  - bars rise over the attack time and fall at a set speed; peak caps hold for a while, then
    fall faster and faster
  - colors: Classic, Prism, Turbo, Viridis, Magma, Gold, Ice or White, by level or by
    frequency; the gap between bars and the size of an LED
  - labels under the bars: the C of each octave, or frequencies; hover to read a band's note,
    frequency and level
- **Oscilloscope**: the left and right waveforms over 1–200 ms, triggered on zero crossings.
- **Vectorscope**: a goniometer (mono is vertical) with a correlation meter.
- **Level meter**: peak, 300 ms RMS, peak hold and a clip light. Click it to reset.

## Groups

A group is a node with a graph inside it. Right-click → **Group** adds one: it has an audio
input and output and a MIDI input and output, and inside, a **Group input** node wired to a
**Group output** node. What reaches the group's inputs comes out of Group input; what reaches
Group output leaves by the group's outputs.

- **Open** on the group (or in its right-click menu) shows the graph inside, to add nodes and
  wire them as in the main graph. The row above it shows where you are: **⬅ Back** goes out
  one level, and the names go back to that level. Each level keeps its own view.
- **Add a node preset inside** adds a node preset's nodes in the group, to wire them to its
  input and output.
- The name field on the group renames it.
- Groups can go inside groups. The Player stays in the main graph; a node preset with a Player
  added inside a group leaves it out.
- Removing a group removes everything in it. Groups are saved with the session, graph presets
  and node presets.
- A plugin's editor inside a group shows while the group is open.

## Presets

- **Graph presets** (💾 Presets tab): "Save" keeps the whole graph under a name, "Load"
  replaces the current graph with one, "✖" deletes (click twice), and "Reset graph" goes back to
  Player → Output (click twice).
- **Node presets**: a few nodes with their wiring and settings, to add again as a group.
  Select them (Shift+drag), right-click the graph and pick "Save the N selected node(s) as a
  node preset…". Add one from the graph menu's "Node presets" (at the clicked spot) or with
  "Add" in the 💾 tab. A Player in a node preset stands for the one already in the graph.

## Recipes

- **Loudness-safe listening**: Player → EQ → Kazu Limiter → Output, with the limiter's
  threshold a few dB below 0.
- **Watching the sound**: branch a Spectrogram off the Player (Player → Spectrogram, with no
  wire out of it) while the Player also goes to the Output.
- **A plugin chain on two paths**: Player → plugin A → Mixer input 1, Player → plugin B → Mixer
  input 2, Mixer → Output, and balance them with the Mixer's gains.

Next: [MIDI](midi.md).
