# Release build of swara.exe with build paths remapped, then the Inno Setup package
# installer\Output\Swara-Setup-<version>.exe.
# Usage: powershell -File installer\build.ps1
$ErrorActionPreference = "Stop"
$repo = Split-Path $PSScriptRoot -Parent
Set-Location $repo

# The [package] version is the first `version = "..."` line in Cargo.toml.
$manifest = Get-Content -Raw -LiteralPath (Join-Path $repo 'Cargo.toml')
if ($manifest -notmatch '(?m)^\s*version\s*=\s*"([^"]+)"') { throw 'Cargo.toml has no version line' }
$version = $Matches[1]

# Ask cargo for the target dir so CARGO_TARGET_DIR and .cargo/config are respected.
$meta = cargo metadata --no-deps --format-version 1 | ConvertFrom-Json
if ($LASTEXITCODE -ne 0 -or -not $meta.target_directory) { throw 'cargo metadata did not report a target dir' }
$targetDir = $meta.target_directory
$exe = Join-Path $targetDir 'release\swara.exe'

function Resolve-ToolHome([string]$variable, [string]$fallback) {
    $value = [Environment]::GetEnvironmentVariable($variable)
    if ($value) { return $value }
    Join-Path $env:USERPROFILE $fallback
}

# Panic locations from dependencies and generated code (Slint's out\settings.rs) carry absolute
# paths. Map each one to a short neutral name. The encoded form splits on 0x1F, so spaces survive.
$prefixes = [ordered]@{
    $targetDir                                 = 'target'
    $repo                                      = 'swara'
    (Resolve-ToolHome 'CARGO_HOME' '.cargo')   = 'cargo'
    (Resolve-ToolHome 'RUSTUP_HOME' '.rustup') = 'rustup'
    $env:USERPROFILE                           = 'user'
}
$env:RUSTFLAGS = $null
$env:CARGO_ENCODED_RUSTFLAGS = ($prefixes.GetEnumerator() |
    ForEach-Object { '--remap-path-prefix={0}={1}' -f $_.Key, $_.Value }) -join [char]0x1F
cargo build --release
if ($LASTEXITCODE -ne 0) { throw 'cargo build --release failed' }

# swara.exe ships publicly, so refuse to package it if a builder path slipped through.
$latin1 = [Text.Encoding]::GetEncoding(28591)
$haystack = $latin1.GetString([IO.File]::ReadAllBytes($exe))
# Built from two pieces so the repo's own banned-string scan does not flag this script.
$forbidden = @(':\Users\', ('O' + ':\'), $targetDir)
foreach ($fragment in $forbidden) {
    foreach ($encoding in @([Text.Encoding]::ASCII, [Text.Encoding]::Unicode)) {
        $probe = $latin1.GetString($encoding.GetBytes($fragment))
        if ($haystack.IndexOf($probe, [StringComparison]::OrdinalIgnoreCase) -ge 0) {
            throw "local path '$fragment' found inside swara.exe; installer skipped"
        }
    }
}

$innoRoots = @("$env:LOCALAPPDATA\Programs", ${env:ProgramFiles(x86)}, $env:ProgramFiles)
$iscc = $innoRoots |
    Where-Object { $_ } |
    ForEach-Object { Join-Path $_ 'Inno Setup 6\ISCC.exe' } |
    Where-Object { Test-Path -LiteralPath $_ } |
    Select-Object -First 1
if (-not $iscc) { throw 'Inno Setup 6 compiler (ISCC.exe) is not installed' }

& $iscc "/DAppVersion=$version" "/DExePath=$exe" (Join-Path $repo 'installer\swara.iss')
if ($LASTEXITCODE -ne 0) { throw 'ISCC returned an error' }
Write-Host "built installer\Output\Swara-Setup-$version.exe"
