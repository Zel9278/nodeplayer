# Run from the repository root after `dotnet tool restore`. One feed per OS/architecture.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string] $Binary,
    [Parameter(Mandatory)][ValidateSet('win-x64', 'linux-x64', 'osx-arm64', 'osx-x64')][string] $Runtime,
    [string] $Version,
    [string] $OutputDir,
    [string] $PreviousReleaseDir
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
$Binary = (Resolve-Path -LiteralPath $Binary).Path
if (!$Version) {
    $Version = [regex]::Match([IO.File]::ReadAllText((Join-Path $root 'Cargo.toml')),
        '(?m)^version = "(\d+\.\d+\.\d+)"').Groups[1].Value
}
if ($Version -notmatch '^\d+\.\d+\.\d+$') { throw 'Version must be major.minor.patch.' }
if (!$OutputDir) { $OutputDir = "target/velopack/$Runtime" }
$null = New-Item -ItemType Directory -Force -Path $OutputDir
$OutputDir = (Resolve-Path -LiteralPath $OutputDir).Path
if ($PreviousReleaseDir) {
    # Only full packages are needed to generate deltas. Do not copy another channel's feed.
    Get-ChildItem -LiteralPath $PreviousReleaseDir -Filter '*-full.nupkg' |
        Copy-Item -Destination $OutputDir
}
$stage = Join-Path $OutputDir ('staging-' + [guid]::NewGuid().ToString('N'))
$null = New-Item -ItemType Directory -Path $stage
$packDir = $stage
$exeName = if ($Runtime -eq 'win-x64') { 'nodeplayer.exe' } else { 'nodeplayer' }
try {
    Copy-Item -LiteralPath $Binary -Destination (Join-Path $stage $exeName)
    Copy-Item -LiteralPath (Join-Path $root 'LICENSE'), (Join-Path $root 'README.md') -Destination $stage
    $vpkArgs = @('pack', '--packId', 'Zel9278.nodeplayer', '--packVersion', $Version,
        '--packTitle', 'nodeplayer', '--packAuthors', 'Zel9278', '--mainExe', $exeName,
        '--runtime', $Runtime, '--channel', $Runtime, '--outputDir', $OutputDir)
    if ($Runtime -eq 'win-x64') {
        $png = [IO.File]::ReadAllBytes((Join-Path $root 'assets/icon.png'))
        $ico = [IO.MemoryStream]::new()
        $writer = [IO.BinaryWriter]::new($ico)
        try {
            $writer.Write([byte[]]@(0, 0, 1, 0, 1, 0, 0, 0, 0, 0, 1, 0, 32, 0))
            $writer.Write([uint32]$png.Length)
            $writer.Write([uint32]22)
            $writer.Write($png)
            [IO.File]::WriteAllBytes((Join-Path $stage 'nodeplayer.ico'), $ico.ToArray())
        } finally { $writer.Dispose(); $ico.Dispose() }
        $vpkArgs += @('--msi', '--instLocation', 'PerUser', '--aumid', 'Zel9278.nodeplayer',
            '--shortcuts', 'StartMenuRoot', '--icon', (Join-Path $stage 'nodeplayer.ico'))
    } elseif ($Runtime.StartsWith('osx-')) {
        # Keep the existing document types, minimum OS and icon. vpk adds its updater
        # inside the app and re-signs the completed bundle with an ad-hoc signature.
        & sh (Join-Path $root 'packaging/macos/make-app.sh') $Binary $Version $stage
        if ($LASTEXITCODE -ne 0) { throw 'macOS app bundle creation failed.' }
        $packDir = Join-Path $stage 'nodeplayer.app'
        Copy-Item -LiteralPath (Join-Path $root 'LICENSE'), (Join-Path $root 'README.md') `
            -Destination (Join-Path $packDir 'Contents/Resources')
        $vpkArgs += @('--bundleId', 'io.github.zel9278.nodeplayer', '--signAppIdentity', '-')
    } else {
        $vpkArgs += @('--icon', (Join-Path $root 'assets/icon.png'), '--categories', 'AudioVideo;Audio;Player;')
    }
    $vpkArgs += @('--packDir', $packDir)
    & dotnet tool run vpk -- --skip-updates --yes --legacyConsole @vpkArgs
    if ($LASTEXITCODE -ne 0) { throw "Velopack packaging failed ($LASTEXITCODE)." }
    $feedPath = Join-Path $OutputDir "releases.$Runtime.json"
    $feed = Get-Content -LiteralPath $feedPath -Raw | ConvertFrom-Json
    $full = @($feed.Assets | Where-Object { $_.Type -eq 'Full' -and $_.Version -eq $Version })
    if ($full.Count -ne 1) { throw 'Missing current full update package in release feed.' }
    foreach ($asset in $feed.Assets) {
        $path = Join-Path $OutputDir $asset.FileName
        if (!(Test-Path -LiteralPath $path)) { throw "Feed references a missing package: $($asset.FileName)" }
        if ((Get-Item -LiteralPath $path).Length -ne $asset.Size) { throw 'Package size differs from feed.' }
        if ($asset.SHA256 -and (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash -ne $asset.SHA256) {
            throw 'Package SHA-256 differs from feed.'
        }
    }
    Write-Host "Velopack package and feed verified: $Runtime $Version"
} finally {
    $resolvedStage = [IO.Path]::GetFullPath($stage)
    $separator = [IO.Path]::DirectorySeparatorChar
    if (!$resolvedStage.StartsWith($OutputDir.TrimEnd($separator) + $separator, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Staging path escaped the output directory.'
    }
    Remove-Item -LiteralPath $resolvedStage -Recurse -Force
}
