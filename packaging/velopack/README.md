# Velopack packaging and updates

Rust crate and `vpk` CLI are both pinned to **1.2.161**. The .NET 8 SDK is only a build
tool; users do not need .NET installed to run nodeplayer or its update helper.

The package ID and Windows AUMID are `Zel9278.nodeplayer`. The macOS bundle ID stays
`io.github.zel9278.nodeplayer`. Channels are `win-x64`, `linux-x64`, `osx-arm64` and
`osx-x64`; keep them stable, because the installed manifest selects the update feed.
Windows packages have a static C++ runtime. Linux packages use the Ubuntu 24.04 build
baseline and still require the usual desktop audio/X11/fontconfig libraries.

## Build locally

Install the .NET 8 SDK, then from the repository root:

```powershell
dotnet tool restore
cargo build --release --locked --target x86_64-pc-windows-msvc
./packaging/velopack/package.ps1 -Binary target/x86_64-pc-windows-msvc/release/nodeplayer.exe -Runtime win-x64
./packaging/velopack/test-update.ps1 -Binary target/x86_64-pc-windows-msvc/release/nodeplayer.exe
```

Run the scripts in PowerShell 7 on macOS/Linux. Use the corresponding compiled binary
and runtime (`osx-arm64`, `osx-x64` or `linux-x64`). macOS packaging must run on a Mac:
the existing app-bundle script preserves Finder file associations, minimum macOS version
and icon. Velopack then embeds its updater and signs the completed bundle ad-hoc.

`package.ps1` reads `Cargo.toml` unless `-Version major.minor.patch` is passed. Outputs
go to `target/velopack/<runtime>` unless `-OutputDir` is specified. Windows produces MSI,
Setup.exe and a self-updating portable ZIP; macOS produces PKG and an app ZIP; Linux
produces AppImage. These files and the matching `releases.<channel>.json` and `.nupkg`
files must all be uploaded to the same GitHub release. The script checks feed file
references, sizes and SHA-256 hashes before reporting success.

For a delta build, use `-PreviousReleaseDir` pointing to a previous package directory,
or use `vpk download github` as in CI. Keep each channel's files separate. The Windows
test builds a temporary next-version package (including a delta), extracts a portable
copy to an isolated tree, and runs the real Update.exe to apply the full package without
launching the normal app or registering shortcuts. Test packages are never published.

## Application integration

`src/updater.rs` handles install/update/uninstall callbacks before ordinary CLI parsing.
Normal GUI startup initializes Velopack before spawning application threads. Diagnostics
and plugin workers skip normal updater startup and network activity. Auto-apply on startup
is disabled, so downloading never implies an unattended install.

Packaged GUI builds check public stable GitHub Releases in a background worker at startup
and every hour while running. Failures retry after an hour. Checks pause while an update
is available, downloading or awaiting restart, and requests cannot overlap.
About provides check/retry, download and restart controls. Only explicit restart launches
the helper, which waits for normal process exit. The app's ordinary shutdown saves state,
stops audio and tears down workers. Custom data-root and in-process plugin options are
passed to the restarted app. A downloaded update is offered again on the next launch.

Bare developer builds have a download link rather than an updater capable of replacing
their executable. Users of pre-Velopack releases must install/extract one Velopack release
first. Existing settings/library paths are unchanged. The installer and app are currently
not signed by a trusted publisher (macOS is ad-hoc signed, not notarized); Windows and
macOS may require the usual first-install confirmation. Update hashes are separate from
OS publisher signing.

## GitHub Actions

Each of the four native platform jobs builds/tests, restores the pinned CLI, optionally
downloads the previous full update package for its channel, and creates packages. Branch
and PR runs validate packaging too. Tag runs upload directly to the draft GitHub release,
so a full Actions artifact quota does not prevent releasing. Channel-specific file names
avoid collisions between concurrent jobs. The draft becomes public only after every
platform's packaging/tests succeed. The app reads public releases without a bundled token.

The first Velopack release has no previous Velopack feed, so it publishes a full package;
later releases can produce deltas automatically. A failure fetching an existing previous
feed is an error, rather than silently turning off deltas.
