# Sharing what's playing

## Misskey NowPlaying

The 📢 Misskey tab posts what's playing to Misskey as a note, like the fooyin plugin
`fooyin_misskey_np` and foobar2000's `foo_misskey_np`.

### Logging in

1. Type the server (for example `misskey.io`) and press **Log in**.
2. The browser opens Misskey's permission page (MiAuth). Allow nodeplayer there; it asks to
   post notes, and to use the Drive for the card image.
3. The account is added to the list. Logging in to the same account again replaces it.

An access token made on the server works too ("Use an access token instead"). Several
accounts can be used at once, each with its own visibility (public, home, followers), "local
only", and on/off.

### When notes are posted

- when a track starts playing (on by default), when pausing, and when stopping (⏹, a media
  key, or the end of the list)
- "Post now" posts the current track by hand
- the same note isn't posted twice in a row, and nothing is posted again within 5 seconds
- posting can be switched off as a whole

### The card image

"Attach a card image" (on by default) adds a Now Playing card: the cover (embedded, or
`cover.jpg`, `folder.png` and the like next to the track), the state (playing, paused with the
time, stopped), title, artist, a progress bar and the album. It goes to the account's Drive, in
a "nodeplayer" folder. If the upload fails, the note is posted without it and the tab says why
(an account logged in before cards existed needs to log in again for Drive access). The preview
in the tab shows the card.

### The template

The note's text uses foobar2000 title formatting:

- `%title%`, `%artist%`, `%album%`, `%album artist%`, `%tracknumber%`, `%year%` and the other
  tags under foobar2000's names
- format info: `%codec%`, `%codec_profile%`, `%bitrate%` (kbps), `%samplerate%`, `%channels%`,
  `%bitspersample%`, `%encoding%` (lossless or lossy), also as `$info(name)`
- play statistics: `%play_count%`, `%first_played%`, `%last_played%`, and `%added%`
- `[...]` shows its contents only when a field inside has a value, `'...'` is literal text
- functions such as `$if`, `$if2`, `$ifequal`, `$ifgreater`, `$left`, `$right`, `$num`,
  `$replace`, `$div` and `$mod`

ⓘ next to the template lists everything, and the preview shows the note for the current track
and its length. Missing fields are empty, and an `@` in a tag can't mention anyone.

### Importing from fooyin

When fooyin's `misskey_nowplaying.conf` exists, "Import from fooyin" copies its accounts,
template and settings (accounts already here are kept).

The settings, tokens included, are stored in `misskey.json` in the settings folder, readable
only by you.

## Discord Rich Presence

The 🎮 Discord tab shows what's playing on your Discord profile as "Listening to", with a
progress bar while it plays. Paused, it shows ⏸ (or nothing, with "Keep showing while paused"
off); stopping clears it. It talks to the Discord app on the same computer (also Vesktop and
the Flatpak and Snap versions on Linux) and connects again by itself when Discord starts later.

### Setting it up

1. Make an application at
   [discord.com/developers/applications](https://discord.com/developers/applications). Its name
   ("nodeplayer", for instance) is what "Listening to" says.
2. Copy its **Application ID** into the tab.
3. Make sure Discord's Settings → Activity Privacy → "Share my activity" is on.

### Texts and cover

- The first line, second line and cover tooltip use the same title formatting as the Misskey
  template (`%title%`, `%artist%` and `%album%` by default). "Member list shows" picks what
  follows "Listening to" in the member list.
- Discord only shows cover images from the web:
  - **Upload the track's own cover to Cloudflare R2** puts the cover (as a JPEG up to 512 px)
    into an R2 bucket and shows it from the bucket's public address. It needs the account ID,
    an R2 API token's access key ID and secret with write access, the bucket, and its public
    URL (its r2.dev address or a custom domain). Each picture is uploaded once.
  - **Otherwise, look the cover up online** (off by default) searches the iTunes Search API by
    artist and album.
- Discord takes only a few updates a minute, so a change can take up to 4 seconds to show.

The settings, the R2 key included, are stored in `discord.json` in the settings folder,
readable only by you.

## The system's media controls

See [Library and playback](library-and-playback.md#media-keys-and-the-systems-media-controls).

Next: [Files and troubleshooting](files-and-troubleshooting.md).
