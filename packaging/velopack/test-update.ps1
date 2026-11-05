# Exercise the real Windows helper against an isolated portable tree. No installation,
# Start menu registration, normal app launch, or external network is used for applying.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string] $Binary,
    [string] $ReleaseDir = 'target/velopack/win-x64'
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$ReleaseDir = (Resolve-Path -LiteralPath $ReleaseDir).Path
$feed = Get-Content (Join-Path $ReleaseDir 'releases.win-x64.json') -Raw | ConvertFrom-Json
$current = $feed.Assets | Where-Object Type -eq 'Full' |
    Sort-Object { [version]$_.Version } -Descending | Select-Object -First 1
if (!$current) { throw 'The test needs a packaged Windows release.' }
$version = [version]$current.Version
$nextVersion = '{0}.{1}.{2}' -f $version.Major, $version.Minor, ($version.Build + 1)
$testDir = Join-Path (Split-Path $ReleaseDir) ('update-test-' + [guid]::NewGuid().ToString('N'))
$nextDir = Join-Path $testDir 'next'
& "$PSScriptRoot/package.ps1" -Binary $Binary -Runtime win-x64 -Version $nextVersion `
    -OutputDir $nextDir -PreviousReleaseDir $ReleaseDir
$portable = @(Get-ChildItem -LiteralPath $ReleaseDir -Filter '*-Portable.zip')
if ($portable.Count -ne 1) { throw 'Expected one Windows portable package.' }
$app = Join-Path $testDir 'portable app'
Expand-Archive -LiteralPath $portable[0].FullName -DestinationPath $app
$manifest = Join-Path $app 'current/sq.version'
[xml]$before = Get-Content -LiteralPath $manifest -Raw
if ($before.package.metadata.version -ne $version.ToString()) { throw 'Wrong starting portable version.' }
$nextFeed = Get-Content (Join-Path $nextDir 'releases.win-x64.json') -Raw | ConvertFrom-Json
$next = $nextFeed.Assets | Where-Object { $_.Type -eq 'Full' -and $_.Version -eq $nextVersion }
if (@($next).Count -ne 1) { throw 'Next full update package is missing.' }
$package = Join-Path $nextDir $next.FileName
$helper = Join-Path $app 'Update.exe'
$packages = Join-Path $app 'packages'
$null = New-Item -ItemType Directory -Force -Path $packages
$process = Start-Process -FilePath $helper -WindowStyle Hidden -PassThru -Wait -ArgumentList @(
    'apply', '--root', "`"$app`"", '--package', "`"$package`"",
    '--packageDir', "`"$packages`"", '--silent', '--norestart')
if ($process.ExitCode -ne 0) { throw "Update helper failed ($($process.ExitCode))." }
[xml]$after = Get-Content -LiteralPath $manifest -Raw
if ($after.package.metadata.version -ne $nextVersion) { throw 'Update did not install the newer version.' }
if ($after.package.metadata.channel -ne 'win-x64') { throw 'Update changed architecture/channel.' }
if ($after.package.metadata.shortcutAumid -ne 'Zel9278.nodeplayer') { throw 'Update lost the app identity.' }
if (!(Test-Path -LiteralPath (Join-Path $app 'current/nodeplayer.exe'))) { throw 'Updated executable is missing.' }
# The new updater package is only for the test: it must not leak into the real feed.
Write-Host "Portable update applied successfully: $version -> $nextVersion"
