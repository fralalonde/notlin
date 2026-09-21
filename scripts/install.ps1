# PowerShell installer for notlin: fetch the release zip and drop notlin.exe
# in %LOCALAPPDATA%\Programs\notlin, and add that dir to the USER-scope PATH
# (Windows has no default user bin dir; registry PATH write IS the mechanism).
# No profile edits. -NoPath skips the PATH write.
# Documented invocation:
#   irm https://github.com/fralalonde/notlin/releases/latest/download/install.ps1 | iex
$ErrorActionPreference = 'Stop'

$repository = if ($env:NOTLIN_REPOSITORY) { $env:NOTLIN_REPOSITORY } else { 'fralalonde/notlin' }
$destDir = if ($env:NOTLIN_DIR) { $env:NOTLIN_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\notlin' }
$version = $env:NOTLIN_VERSION

if (-not $version) {
    $latest = Invoke-RestMethod -Uri "https://api.github.com/repos/$repository/releases/latest"
    $version = $latest.tag_name.TrimStart('v')
}
if (-not $version) { throw 'Unable to determine notlin version; set NOTLIN_VERSION=x.y.z.' }

$asset = "notlin-$version-x86_64-pc-windows-msvc.zip"
$base = "https://github.com/$repository/releases/download/v$version"
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("notlin-install-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    $archive = Join-Path $tmp $asset
    Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset" -OutFile $archive

    # Verify checksum when a checksums.txt is published (best-effort fetch,
    # fatal on mismatch).
    try {
        $checksums = Join-Path $tmp 'checksums.txt'
        Invoke-WebRequest -UseBasicParsing -Uri "$base/checksums.txt" -OutFile $checksums
        $expected = ((Get-Content $checksums | Where-Object { $_ -match ([regex]::Escape($asset) + '$') }) -split '\s+')[0]
        if ($expected -and (Get-FileHash -Algorithm SHA256 $archive).Hash.ToLowerInvariant() -ne $expected.ToLowerInvariant()) {
            throw 'Archive checksum does not match checksums.txt.'
        }
    } catch [System.Net.WebException] { }

    Expand-Archive -Path $archive -DestinationPath $tmp -Force
    New-Item -ItemType Directory -Force -Path $destDir | Out-Null
    # The archive contains the notlin.exe at its root (see release.yml).
    $exe = Get-ChildItem -Path $tmp -Filter 'notlin.exe' -Recurse | Select-Object -First 1
    if (-not $exe) { throw 'Release archive is missing notlin.exe' }
    Copy-Item $exe.FullName (Join-Path $destDir 'notlin.exe') -Force

    Write-Host ""
    Write-Host "Installed notlin $version to $destDir\notlin.exe"
    # Windows has no default user bin dir on PATH; the OS-native equivalent of
    # the Unix "~/.local/bin" convention is adding the dir to the USER-scope
    # PATH via the registry. Default: do it. -NoPath opts out (then the
    # installer prints the manual path syntax instead of editing anything).
    if ($NoPath) {
        Write-Host "Set $destDir on your PATH to use notlin."
    } else {
        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        $parts = @()
        if ($userPath) { $parts = $userPath -split ';' | Where-Object { $_ } }
        if ($parts -notcontains $destDir) {
            $newPath = (@($parts) + $destDir) -join ';'
            [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
            Write-Host "Added $destDir to user PATH (new terminals only)."
        }
    }
    Write-Host "Run: notlin --help   (new terminal if PATH was just modified)"
} finally {
    if (Test-Path $tmp) { Remove-Item -Recurse -Force $tmp }
}
