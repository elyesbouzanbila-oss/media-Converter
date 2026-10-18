# Fetches the PDFium binaries this project binds against.
#
# PDFium is not on crates.io as a static library and we do not build it from
# source (that needs depot_tools + GN/Ninja and roughly 10GB of toolchain). The
# bblanchon/pdfium-binaries project republishes Google's official Chromium
# builds, which is what we vendor here.
#
# The download is deliberately NOT committed. Binaries live in
# third_party/pdfium/ and are gitignored; run this script after a fresh clone.
#
#   .\scripts\fetch-pdfium.ps1
#
# The pinned version is also embedded into the final application binary via
# include_bytes! (engine/pdf/build.rs), and extracted to a version-keyed cache
# directory on first run (engine/pdf/src/bind.rs). Keep $PdfiumVersion in sync
# with PDFIUM_VERSION in that file: it names the cache directory, so a mismatch
# means the same version is fetched twice and the old library stays on disk.
#
# Fetching *after* a build is the case this script has to work for, and cargo
# does not make that automatic - see Update-EmbedStamp below.

[CmdletBinding()]
param(
    [string]$PdfiumVersion = '8100',
    [switch]$Force
)

$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
$dest = Join-Path $repoRoot 'third_party\pdfium'

# The tarball while it is being unpacked, and the stamp saying which version is
# unpacked. Ignored by .gitignore, and deliberately not the tarball's own VERSION
# file: that one is committed along with the licence texts, and overwriting it
# turns up as a modified file in every working tree that has run this script.
$cache = Join-Path $dest '.cache'

# Make the next build notice the library.
#
# Cargo decides whether to re-run engine/pdf/build.rs by comparing the watched
# file's timestamp against the last build's, and `tar` restores the archive's -
# so a library this script vendors is *older* than the build it has to
# invalidate. The build after it then still carries no copy of PDFium, and says
# nothing: the only symptom is an executable that wants a DLL beside it. (This
# was measured, not deduced: a build made with no library, followed by one made
# with a thirty-day-old copy in place, reports "Fresh dconv-pdf".)
#
# Both lines exist because they fail differently. Setting the library's own
# timestamp makes it newer than any build that came before, which is what the
# watch on it asks for. Touching build.rs dirties the crate as a source change,
# which cargo cannot miss however it reasons about a path that did not exist the
# last time it looked. Neither is expensive; the failure is silent, so it is
# worth both.
function Update-EmbedStamp {
    $library = Join-Path $dest 'bin\pdfium.dll'
    if (Test-Path $library) {
        (Get-Item $library).LastWriteTime = Get-Date
    }

    $buildScript = Join-Path $repoRoot 'engine\pdf\build.rs'
    if (Test-Path $buildScript) {
        (Get-Item $buildScript).LastWriteTime = Get-Date
    }
}

# The pieces the rest of the project needs to be present after an unpack. Used
# both after extracting and before trusting the stamp, so a stamp cannot claim a
# library that is not there - the embed and the runtime search both trust the
# directory it describes.
$required = @(
    'bin\pdfium.dll',
    'lib\pdfium.dll.lib',
    'include\fpdfview.h',
    'include\fpdf_text.h'
)

function Get-MissingFile {
    $required | Where-Object { -not (Test-Path (Join-Path $dest $_)) }
}

if ($PdfiumVersion -ne '8100') {
    Write-Warning "pdfium-render 0.9.4 generates bindings for the chromium/7881 API."
    Write-Warning "Versions newer than that are a superset and safe; older ones are not."
}

$versionStamp = Join-Path $cache 'VERSION'
if ((Test-Path $versionStamp) -and -not $Force) {
    $existing = (Get-Content $versionStamp -Raw).Trim()
    $missing = Get-MissingFile
    if ($existing -ne $PdfiumVersion) {
        Write-Host "PDFium version changed ($existing -> $PdfiumVersion), refetching" -ForegroundColor Yellow
    }
    elseif ($missing) {
        Write-Host "PDFium $PdfiumVersion is stamped as fetched, but $($missing -join ', ') is missing - refetching" -ForegroundColor Yellow
    }
    else {
        Update-EmbedStamp
        Write-Host "PDFium $PdfiumVersion already present at $dest" -ForegroundColor Green
        Write-Host "  (engine/pdf/build.rs touched, so the next build embeds it)" -ForegroundColor Green
        return
    }
}

New-Item -ItemType Directory -Force -Path $cache | Out-Null

# The tag contains a slash, which must be percent-encoded in the URL path.
$tag = [System.Uri]::EscapeDataString("chromium/$PdfiumVersion")
$asset = 'pdfium-win-x64.tgz'
$url = "https://github.com/bblanchon/pdfium-binaries/releases/download/$tag/$asset"

$tgz = Join-Path $cache $asset
Write-Host "Downloading $url"
try {
    Invoke-WebRequest -Uri $url -OutFile $tgz -Headers @{ 'User-Agent' = 'docconvert-build' }

    Write-Host "Extracting to $dest"
    tar -xzf $tgz -C $dest
    if ($LASTEXITCODE -ne 0) { throw "tar failed with exit code $LASTEXITCODE" }
}
finally {
    # In .cache/ so that a failed download leaves nothing git can see, and
    # removed either way so thirty megabytes do not sit there afterwards.
    Remove-Item $tgz -Force -ErrorAction SilentlyContinue
}

# A silent upstream layout change fails here rather than as a confusing linker
# or runtime error later.
$missing = Get-MissingFile
if ($missing) {
    throw "PDFium package is missing expected files: $($missing -join ', ')"
}

Set-Content -Path $versionStamp -Value $PdfiumVersion

# Last, so the timestamps this script leaves behind are the newest thing it did.
Update-EmbedStamp

$dll = Get-Item (Join-Path $dest 'bin\pdfium.dll')
Write-Host ("PDFium {0} ready: {1:N0} bytes" -f $PdfiumVersion, $dll.Length) -ForegroundColor Green
Write-Host "  engine/pdf/build.rs touched, so the next build embeds it" -ForegroundColor Green
