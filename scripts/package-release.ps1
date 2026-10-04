param([string]$Version)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location -LiteralPath $root

$manifest = Get-Content -LiteralPath 'Cargo.toml' -Raw
if ($manifest -notmatch '(?m)^version = "(\d+\.\d+\.\d+)"\r?$') {
    throw 'Could not read the package version from Cargo.toml'
}
$packageVersion = $Matches[1]
if (-not $Version) { $Version = $packageVersion }
if ($Version -ne $packageVersion) { throw "Tag version $Version does not match Cargo.toml $packageVersion" }

$binary = 'target/release/bibiiwiki.exe'
if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) { throw "Missing $binary" }

$stage = 'dist/.work/windows'
New-Item -ItemType Directory -Path $stage -Force | Out-Null
Copy-Item -LiteralPath $binary, 'README.md', 'LICENSE', 'bibiiwiki.example.yaml' -Destination $stage
$archive = "dist/bibiiwiki-$Version-windows-x86_64.zip"
Compress-Archive -Path "$stage/*" -DestinationPath $archive -Force
if ((Get-Item -LiteralPath $archive).Length -eq 0) { throw "Empty archive: $archive" }
Write-Host "Packaged $archive"
