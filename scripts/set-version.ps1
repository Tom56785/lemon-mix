# Sets the version everywhere it is written down: the workspace's
# Cargo.toml (which the executables report and embed in their file
# properties), Cargo.lock, and the Stream Deck plugin's package.json,
# package-lock.json and manifest.json. The release workflow runs it on the
# runner; it can also be run by hand:
#
#     powershell -ExecutionPolicy Bypass -File scripts\set-version.ps1 1.2.3
#
# The version is MAJOR.MINOR.PATCH, optionally with a pre-release suffix
# such as 1.2.3-dev, which the plugin's four-number version leaves out.

param(
    [Parameter(Mandatory = $true)][string]$Version
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$Version = $Version.Trim().TrimStart('v')
if ($Version -notmatch '^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(-[0-9A-Za-z.-]+)?$') {
    throw "'$Version' is not a version of the form MAJOR.MINOR.PATCH (with an optional -suffix)."
}
$release = "$($Matches[1]).$($Matches[2]).$($Matches[3])"
$utf8 = New-Object System.Text.UTF8Encoding $false

function Update-File([string]$path, [string]$pattern, [string]$replacement) {
    $full = Join-Path $root $path
    $text = [IO.File]::ReadAllText($full)
    $regex = [regex]::new($pattern, 'Multiline')
    if (-not $regex.IsMatch($text)) { throw "No version found in $path." }
    [IO.File]::WriteAllText($full, $regex.Replace($text, $replacement, 1), $utf8)
}

# The workspace version, in [workspace.package]: the first version line.
Update-File 'Cargo.toml' '^version = "[^"]*"' "version = `"$Version`""
Push-Location $root
try {
    # Records the new version in Cargo.lock without changing dependencies.
    # Cargo reports progress on stderr, which Windows PowerShell would turn
    # into an error under 'Stop'.
    $ErrorActionPreference = 'Continue'
    cargo update --workspace --offline --quiet
    if ($LASTEXITCODE -ne 0) { cargo update --workspace --quiet }
    if ($LASTEXITCODE -ne 0) { throw 'cargo update failed' }
    $ErrorActionPreference = 'Stop'
} finally {
    Pop-Location
}

# The plugin: npm's version, and Stream Deck's four numbers.
Update-File 'streamdeck/package.json' '^(\s*)"version": "[^"]*"' "`$1`"version`": `"$release`""
Update-File 'streamdeck/package-lock.json' '^(\s*)"version": "[^"]*"' "`$1`"version`": `"$release`""
Update-File 'streamdeck/package-lock.json' '("packages": \{\s*"": \{\s*"name": "[^"]*",\s*)"version": "[^"]*"' "`$1`"version`": `"$release`""
Update-File 'streamdeck/com.lemonmix.mixer.sdPlugin/manifest.json' '^(\s*)"Version": "\d+\.\d+\.\d+\.\d+"' "`$1`"Version`": `"$release.0`""

Write-Host "Version set to $Version (plugin $release.0)"
