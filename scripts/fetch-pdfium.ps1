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
# include_bytes!, and extracted to a version-keyed cache directory on first run.
# Keep $PdfiumVersion in sync with the constant in crates/pdfium-sys/build.rs.

[CmdletBinding()]
param(
    [string]$PdfiumVersion = '8100',
    [switch]$Force
)

$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
$dest = Join-Path $repoRoot 'third_party\pdfium'

if ($PdfiumVersion -ne '8100') {
    Write-Warning "pdfium-render 0.9.4 generates bindings for the chromium/7881 API."
    Write-Warning "Versions newer than that are a superset and safe; older ones are not."
}

$versionStamp = Join-Path $dest 'VERSION'
if ((Test-Path $versionStamp) -and -not $Force) {
    $existing = (Get-Content $versionStamp -Raw).Trim()
    if ($existing -eq $PdfiumVersion) {
        Write-Host "PDFium $PdfiumVersion already present at $dest" -ForegroundColor Green
        return
    }
    Write-Host "PDFium version changed ($existing -> $PdfiumVersion), refetching" -ForegroundColor Yellow
}

New-Item -ItemType Directory -Force -Path $dest | Out-Null

# The tag contains a slash, which must be percent-encoded in the URL path.
$tag = [System.Uri]::EscapeDataString("chromium/$PdfiumVersion")
$asset = 'pdfium-win-x64.tgz'
$url = "https://github.com/bblanchon/pdfium-binaries/releases/download/$tag/$asset"

$tgz = Join-Path $dest $asset
Write-Host "Downloading $url"
Invoke-WebRequest -Uri $url -OutFile $tgz -Headers @{ 'User-Agent' = 'docconvert-build' }

Write-Host "Extracting to $dest"
tar -xzf $tgz -C $dest
if ($LASTEXITCODE -ne 0) { throw "tar failed with exit code $LASTEXITCODE" }
Remove-Item $tgz -Force

# Sanity-check the pieces we actually depend on, so a silent upstream layout
# change fails here rather than as a confusing linker or runtime error later.
$required = @(
    'bin\pdfium.dll',
    'lib\pdfium.dll.lib',
    'include\fpdfview.h',
    'include\fpdf_text.h'
)
$missing = $required | Where-Object { -not (Test-Path (Join-Path $dest $_)) }
if ($missing) {
    throw "PDFium package is missing expected files: $($missing -join ', ')"
}

Set-Content -Path $versionStamp -Value $PdfiumVersion

$dll = Get-Item (Join-Path $dest 'bin\pdfium.dll')
Write-Host ("PDFium {0} ready: {1:N0} bytes" -f $PdfiumVersion, $dll.Length) -ForegroundColor Green
