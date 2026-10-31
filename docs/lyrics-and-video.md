# Lyrics and video

## Lyrics

Add a **Lyrics** node (right-click the graph). It shows the current track's lyrics; it needs
no wires.

- Timed lyrics (LRC) follow playback: the current line is highlighted and kept near the
  middle, clicking a line seeks there, and the wheel scrolls by hand (following resumes after
  4 seconds). **offset** shifts the timing when the lyrics run early or late.
- Plain lyrics just scroll.
- **size** sets the font size; the box resizes from its corner.

The lyrics come from the first of these that has them:

1. an `.lrc` (or `.txt`) file next to the track with the same name
2. lyrics in the file's tags
3. with **online** on: NetEase Cloud Music, then Kugou, searched by title and artist and
   matched by name and length. These are the services' unofficial web APIs, so they may stop
   working; the title, artist and length are sent to music.163.com and kugou.com. Answers are
   cached; "not found" is asked again after a week, or at once with ⟳.

## Video

Add a **Video** node to see the picture of a video file the Player plays (mp4, mkv, webm, mov
and anything else FFmpeg reads). It needs FFmpeg (see
[Getting started](getting-started.md#ffmpeg)).

- The picture follows the player: pausing, seeking, the playback rate and the next track.
  The sound still comes from the Player node through the graph; a video without sound plays
  silence for its length.
- The size menu picks the decoded height (360p–1080p or original; 720p by default). Phone
  videos are turned upright.
- **offset** shifts the picture against the sound.
- **window**, or double-clicking the picture, also shows it in a window of its own. There,
  double-click or F / F11 toggles full screen, Esc leaves it, Space pauses, and ← / → seek 5
  seconds.
- The box resizes from its corner.

Next: [Sharing what's playing](sharing.md).
