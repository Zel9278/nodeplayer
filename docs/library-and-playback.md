# Library and playback

## The library

The 🎵 Library tab is the track list.

- **Adding music**: "📁 Add folder…" adds folders with their subfolders, "🎵 Add files…" adds
  single files, and dropping a folder onto the window adds it too. The sources are listed
  under the buttons (✖ removes one). ⟳ scans them again.
- **What is listed**: audio files (flac, mp3, wav, ogg, opus, m4a, aac, aiff, caf, mkv, webm),
  video files (mp4, mkv, webm, mov, avi and others) and MIDI files (mid, midi).
- **Columns**: track number, title, artist, album and length, read from the tags in the
  background. Tags are cached, so only new or changed files are read again. Three more columns
  can be shown: the release date (the `date` tag), when the file was added (its creation time,
  else its modification time) and the play count.
- **Sorting**: click a heading to sort by it, again for descending, a third time for folder
  order (with folder headings). Right-click the headings to show or hide columns, or to sort by
  any column ("Sort by"). Drag a heading's right edge to resize the column. Playback follows the
  order shown.
- **Filter**: matches file names, folders, titles, artists and albums.
- **Play counts**: a play counts once half the track, or four minutes of it, has been listened
  to. Seeking doesn't count.

## The transport

| Control | What it does |
|---|---|
| 📂 Open | Plays a file without adding it to the library |
| ⏮ | Restarts the track if more than 3 seconds have played, else goes to the previous one |
| ▶ / ⏸ | Play and pause (also Space) |
| ⏹ | Stop and go back to the start |
| ⏭ | Next track |
| Repeat | Off, all, one, or selection (the button shows which); see [Looping a selection](#looping-a-selection) |
| 〰 | Opens the loop editor |
| Shuffle | Plays the list in random order |
| Position bar | Click or drag to seek; a line under it marks the selection (blue while it loops) |

## Looping a selection

The repeat mode **selection** plays a stretch of the track over and over. The jump back has no
gap: the decoder goes back to the start itself, and the few milliseconds after the end fade
out under the start, so there is no click. Without a selection, the whole track loops without
a gap. MIDI files loop the same way, with the notes timed across the jump.

The selection is made in the **loop editor** (〰 next to the repeat button). It's a window of
its own that draws the track's waveform like an oscilloscope, left channel over right:

- **Drag** across the waveform to select. **Drag an edge** to move it, **Ctrl+drag** inside
  the selection to move all of it, and **click** to seek. The ruler above seeks too.
- **The wheel** zooms in and out around the pointer, **Shift+wheel** or **middle-drag** scrolls,
  and **Ctrl+wheel** scales the height. Zoomed in far enough, the samples themselves are drawn,
  joined by a line, and as dots when they're far apart.
- The strip at the top shows the whole track: click or drag it to go there.
- **Start** and **End** can be dragged or typed (`m:ss.mmm`). **Set start** and **Set end**
  put them at the playhead.
- **Loop selection** turns the loop on and off; off brings back the repeat mode from before.
- **Follow playback** keeps the playhead in view. For MIDI files, which have no waveform, the
  editor draws the bars and beats instead, and **Snap to beats** pulls the edges onto them.
- **Presets** (the panel on the right) keep selections of the track under a name: type one
  (or leave it as `Loop 1`, `Loop 2`, …) and press **Save selection** or Enter. Click a preset
  to select it again, or press its number key (1–9); ✖ deletes it. Saving under a name already
  there replaces that preset.
- Keys: Space plays and pauses, L turns the loop on and off, A and B set the start and end at
  the playhead, Z zooms to the selection, F shows the whole track, Esc clears the selection, and
  1–9 select the presets.

Turning the loop on, or changing the selection, while the playhead is outside it goes to its
start. Seeking past the end on purpose plays on to the end of the track, and then goes back to
the selection's start.

Each track keeps its own selection and presets, also after nodeplayer is closed: coming back to
a track brings its selection back. They are stored in `loops.json` in the settings folder, by
the file's path, so a track that is moved or renamed starts without them.

## The playback rate

The `1.00×` next to the time shifts the playback rate between 0.5× and 2×. Speed and pitch
change together, like a tape played faster or slower; its tooltip gives the change in
semitones.

- Drag it, or double-click it to type a value. Right-click resets it to 1×.
- A change is heard at once and glides while you drag.
- The Video and Lyrics panels follow the rate, and MIDI files change speed and pitch with it
  (see [MIDI](midi.md#the-playback-rate)).
- The same shift is a node too, **Playback Rate Shift**, with a slider and bypass. It counts
  while the Player's audio goes through it (the node says when it doesn't), several in a row
  multiply, and the transport's rate multiplies them. The result, capped at 0.5–2×, is shown
  after the transport's value (`→ 1.35×` in the screenshot).
- At exactly 1× the audio passes through untouched.

## Now playing

The ℹ Now playing tab tells what is playing and how:

- the bitrate of the second just played, with a graph of the last minute against the file's
  average (hover the graph for a value; files FFmpeg plays only show the average)
- codec and profile, lossless or lossy, bit depth, the sample rate and whether it is
  resampled for the output, the channels, the playback rate, the decoder and the output device
- for MIDI files: the format, tracks, notes, resolution and tempo changes instead
- the file (folder, container, size, when it was added), its play statistics and every tag

Values can be selected and copied.

## Media keys and the system's media controls

nodeplayer shows up in the system's media controls and answers media keys: MPRIS on Linux
(KDE, GNOME and the like), the media overlay on Windows, and Now Playing on macOS. They show
the title, artist, album, length and cover, and can play, pause, stop, skip and seek.

Next: [The node graph](node-graph.md).
