# scry poller, Windows port of poll.sh: a loop over `scry --tick`, loading
# `.env` from the repo root first. Same environment surface as poll.sh
# (see its header), except SCRY_BIN is a path to the binary rather than a
# command line and defaults to the release build.
#
# UNTESTED: written alongside highlight.ps1 without a Windows machine.
#
# Usage:
#   powershell -File scripts\poll.ps1          # loop forever, every $env:SCRY_INTERVAL seconds
#   powershell -File scripts\poll.ps1 -Once    # a single pass, then exit

param([switch]$Once)

$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$envFile = Join-Path $root '.env'
if (Test-Path -LiteralPath $envFile) {
    foreach ($line in Get-Content -LiteralPath $envFile) {
        if ($line -match '^\s*([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.*?)\s*$') {
            [Environment]::SetEnvironmentVariable($Matches[1], $Matches[2].Trim('"'), 'Process')
        }
    }
}

$interval = if ($env:SCRY_INTERVAL) { [int]$env:SCRY_INTERVAL } else { 300 }
$scry = if ($env:SCRY_BIN) { $env:SCRY_BIN } else { Join-Path $root 'target\release\scry.exe' }
$flags = @('--tick')
if ($env:SCRY_CLIPS -eq '0') { $flags += '--no-clips' }

while ($true) {
    & $scry @flags
    if ($Once) { break }
    Start-Sleep -Seconds $interval
}
