# Assembles the distributable: the two executables, and the notices that travel
# with them.
#
#   .\scripts\package.ps1                  # the deliverable: --profile release
#   .\scripts\package.ps1 -Profile spike   # same assembly, faster link
#
# Why the notices are files beside the executables rather than something the
# executables print: each .exe is self-sufficient at run time - PDFium is
# embedded and unpacked on first use - so the texts *could* have been embedded
# too, and printed by a `--licenses` flag. That was the alternative considered
# and rejected: a redistributor has to be able to pass the notices on, and a
# folder is easier to pass on than a flag. The cost is that the deliverable is a
# folder and not one file, which is why it is worth saying plainly instead of
# leaving it to be discovered.
#
# What this is not: a complete third-party notice bundle. It carries what the
# library inside the binaries requires. The notices for the Rust crates this
# build links are not collected yet - the gap list in AGENTS.md has where that
# stands.
#
# The last check is the one this script exists for. An .exe that needs a
# pdfium.dll beside it, shipped by a script that reported success, is exactly the
# failure the embed was built to prevent - so each executable must contain the
# vendored library's bytes, and the check runs before anything is copied.

[CmdletBinding()]
param(
    [string]$Profile = 'release',
    [string]$Output = 'dist',
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
$thirdParty = Join-Path $repoRoot 'third_party\pdfium'
$libraryPath = Join-Path $thirdParty 'bin\pdfium.dll'
$dist = Join-Path $repoRoot $Output

# Cargo names the output directory after the profile, except for `dev`, whose
# directory is `debug` for historical reasons.
$profileDir = if ($Profile -eq 'dev') { 'debug' } else { $Profile }
$buildDir = Join-Path $repoRoot "target\$profileDir"

$binaries = @('dconv', 'dconvert')

# Where $Needle sits inside $Path, or -1.
#
# The bytes go through ISO-8859-1, the one encoding that maps a byte to a
# character one for one, and are searched as a string: .NET's ordinal search is
# fast enough for a 17 MB haystack, where comparing megabytes in a PowerShell
# loop is not. A hash cannot answer this question - the library is inside the
# executable, not beside it, so there is no second file to hash.
function Find-Bytes {
    param([string]$Path, [byte[]]$Needle)

    $encoding = [System.Text.Encoding]::GetEncoding(28591)
    $haystack = $encoding.GetString([System.IO.File]::ReadAllBytes($Path))
    return $haystack.IndexOf($encoding.GetString($Needle), [System.StringComparison]::Ordinal)
}

# Which PDFium this build binds, read from the constant the embed, the cache
# directory and the fetch script all use - so the README cannot end up
# describing a different version from the one inside the executables.
$bindSource = Get-Content (Join-Path $repoRoot 'engine\pdf\src\bind.rs') -Raw
if ($bindSource -notmatch 'PDFIUM_VERSION: &str = "([0-9.]+)"') {
    throw 'could not read PDFIUM_VERSION from engine/pdf/src/bind.rs'
}
$pdfiumVersion = $Matches[1]

# The library has to be in place *before* the build, because build.rs decides
# whether to embed it at compile time. The fetch script also touches the build
# script, because cargo would otherwise miss a library that arrived after the
# last build - see Update-EmbedStamp in that script.
Write-Host '== PDFium ==' -ForegroundColor Cyan
& (Join-Path $PSScriptRoot 'fetch-pdfium.ps1')

if (-not (Test-Path $libraryPath)) {
    throw "$libraryPath is missing after the fetch - nothing to embed"
}

if (-not $SkipBuild) {
    Write-Host "== Building --profile $Profile ==" -ForegroundColor Cyan
    cargo build --profile $Profile -p dconv-cli -p dconv-app --manifest-path (Join-Path $repoRoot 'Cargo.toml')
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }
}

Write-Host '== Checking that the library is inside the executables ==' -ForegroundColor Cyan
$libraryBytes = [System.IO.File]::ReadAllBytes($libraryPath)
$embeddedAt = @{}

foreach ($name in $binaries) {
    $exe = Join-Path $buildDir "$name.exe"
    if (-not (Test-Path $exe)) {
        throw "$exe is missing - build --profile $Profile first, or drop -SkipBuild"
    }

    $at = Find-Bytes -Path $exe -Needle $libraryBytes
    if ($at -lt 0) {
        throw ("$name.exe does not carry PDFium, so it would need a pdfium.dll beside it. " +
            'Run scripts/fetch-pdfium.ps1, touch engine/pdf/build.rs if it was already fetched, and build again.')
    }

    $embeddedAt[$name] = $at
    Write-Host ("  {0,-12} carries {1:N0} bytes at offset {2:N0}" -f "$name.exe", $libraryBytes.Length, $at)
}

Write-Host "== Assembling $Output ==" -ForegroundColor Cyan
if (Test-Path $dist) { Remove-Item $dist -Recurse -Force }
$licenses = Join-Path $dist 'LICENSES'
New-Item -ItemType Directory -Force -Path $licenses | Out-Null

foreach ($name in $binaries) {
    Copy-Item (Join-Path $buildDir "$name.exe") (Join-Path $dist "$name.exe")
}

# Verbatim, upstream file names: a notice that has been reformatted or renamed is
# harder to compare against the distribution it came from, and this is the set a
# redistributor has to pass on unchanged.
Copy-Item (Join-Path $thirdParty 'licenses\*') $licenses

# The top-level LICENSE of the PDFium *distribution* is the repackager's own
# (Benoit Blanchon, MIT), not PDFium's - hence the name it gets here, and the
# sentence in the README that says so. PDFium itself is licenses/pdfium.txt.
Copy-Item (Join-Path $thirdParty 'LICENSE') (Join-Path $licenses 'pdfium-binaries.txt')

$readme = @'
Third-party notices for dconv.exe and dconvert.exe
==================================================

dconv.exe and dconvert.exe, one directory up from this file, each contain a copy
of PDFium - the PDF renderer by The PDFium Authors - and unpack it into
%LOCALAPPDATA%\docconvert\pdfium\{VERSION}\ the first time they run. Nothing
beside these files is needed at run time, so this directory can be moved or
copied anywhere instead of installed.

The texts here are the notices that library and its dependencies require, copied
unmodified from the PDFium {VERSION} binary distribution
(github.com/bblanchon/pdfium-binaries), whose own licence is
pdfium-binaries.txt. pdfium.txt is PDFium itself; the remaining files belong to
the components it bundles.

Not collected yet: the notices for the Rust libraries these programs link
(egui, typst, serde and the rest). Anyone redistributing the executables needs
that set as well.
'@ -replace '\{VERSION\}', $pdfiumVersion

# Written as UTF-8 without a byte-order mark: -Encoding UTF8 in Windows
# PowerShell prepends one, and a text file that starts with three invisible bytes
# is a papercut for whoever opens it next.
[System.IO.File]::WriteAllText(
    (Join-Path $licenses 'README.txt'),
    ($readme -replace "(?<!\r)\n", "`r`n"),
    (New-Object System.Text.UTF8Encoding($false))
)

Write-Host ''
Write-Host "== $Output ==" -ForegroundColor Green
Get-ChildItem $dist -File | ForEach-Object { '  {0,-14} {1,12:N0} bytes' -f $_.Name, $_.Length }
Write-Host ('  {0,-14} {1,12:N0} files' -f 'LICENSES\', (Get-ChildItem $licenses).Count)
Write-Host ''
Write-Host '  Each executable found its own PDFium:' -ForegroundColor Green
foreach ($name in $binaries) {
    Write-Host ('    {0,-14} PDFium {1} at offset {2:N0}' -f "$name.exe", $pdfiumVersion, $embeddedAt[$name])
}
