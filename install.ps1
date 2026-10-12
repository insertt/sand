# Installs a prebuilt `sand` executable from GitHub Releases (Windows).
#
#   irm https://raw.githubusercontent.com/insertt/sand/main/install.ps1 | iex
#
# Environment:
#   SAND_VERSION      release to install, e.g. v0.1.0 (default: latest)
#   SAND_INSTALL_DIR  where to put the executable (default: %LOCALAPPDATA%\Programs\sand)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$repo = 'insertt/sand'
$version = if ($env:SAND_VERSION) { $env:SAND_VERSION } else { 'latest' }
$installDir = if ($env:SAND_INSTALL_DIR) { $env:SAND_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\sand' }

# There is no native ARM64 build; Windows on ARM runs the x64 one emulated.
$asset = 'sand-x86_64-pc-windows-msvc.zip'
$url = if ($version -eq 'latest') {
    "https://github.com/$repo/releases/latest/download/$asset"
} else {
    "https://github.com/$repo/releases/download/$version/$asset"
}

$tmp = Join-Path ([IO.Path]::GetTempPath()) ([IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    Write-Host "Downloading $url"
    $zip = Join-Path $tmp $asset
    Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
    Invoke-WebRequest -Uri "$url.sha256" -OutFile "$zip.sha256" -UseBasicParsing

    $expected = ((Get-Content "$zip.sha256" -Raw).Trim() -split '\s+')[0]
    $actual = (Get-FileHash $zip -Algorithm SHA256).Hash
    if ($actual -ne $expected) {
        throw "checksum mismatch for $asset"
    }

    Expand-Archive -Path $zip -DestinationPath $tmp -Force
    New-Item -ItemType Directory -Path $installDir -Force | Out-Null
    $exe = Join-Path $installDir 'sand.exe'
    Copy-Item (Join-Path $tmp 'sand.exe') $exe -Force
} finally {
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host "Installed $(& $exe --version) to $exe"

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($userPath -split ';') -notcontains $installDir) {
    [Environment]::SetEnvironmentVariable('Path', "$userPath;$installDir".TrimStart(';'), 'User')
    Write-Host "Added $installDir to your user PATH; open a new terminal to use ``sand``."
}
