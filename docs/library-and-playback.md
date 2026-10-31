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
| Repeat | Off, all, one (the button shows which) |
| Shuffle | Plays the list in random order |
| Position bar | Click or drag to seek |

## The playback rate

The `1.00×` next to the time shifts the playback rate between 0.5× and 2×. Speed and pitch
change together, like a tape played faster or slower; its tooltip gives the change in
semitones.

- Drag it, or double-click it to type a value. Right-click resets it to 1×.
- A change is heard at once and glides while you drag.
- The Video and Lyrics nodes follow the rate, and MIDI files change speed and pitch with it
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
