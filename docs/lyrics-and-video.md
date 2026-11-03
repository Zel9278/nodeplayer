# Lyrics and video

## Lyrics

The **🎤 Lyrics** panel shows the current track's lyrics. Open it from the "+" of any tab bar,
and dock it where you like, as with the other panels.

- Timed lyrics (LRC) follow playback: the current line is highlighted and kept near the
  middle, clicking a line seeks there, and the wheel scrolls by hand (following resumes after
  4 seconds). **offset** shifts the timing when the lyrics run early or late; ↺ sets it back
  to 0.
- Plain lyrics just scroll.
- **size** sets the font size; the lyrics fill the panel.

The menu at the top picks where the lyrics come from. **auto** takes the first of these that
has them; picking one uses only that one:

1. **file**: an `.lrc` (or `.txt`) file next to the track with the same name
2. **embedded**: lyrics in the file's tags
3. with **online** on: **NetEase** (NetEase Cloud Music), **Kugou**, then **LRCLIB**
   (lrclib.net), searched by title and artist and matched by name and length. NetEase's and
   Kugou's are the services' unofficial web APIs, so they may stop working; LRCLIB's is public.
   The title, artist and length are sent to music.163.com, kugou.com and lrclib.net. Each
   one's answers are cached; "not found" is asked again after a week, or at once with ⟳.

The provider picked and the offset are kept for each track, in `lyrics.json` in the settings
folder, and come back when the track plays again.

## Video

The **🎬 Video** panel (from a tab bar's "+") shows the picture of a video file the Player
plays (mp4, mkv, webm, mov and anything else FFmpeg reads). It needs FFmpeg (see
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
- The picture fills the panel.

Graphs saved by earlier versions had Lyrics and Video nodes; their settings go to the panels,
which open by themselves the first time.

Next: [Sharing what's playing](sharing.md).
