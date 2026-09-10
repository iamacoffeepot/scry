# Record the Highlight and Lowlight clips for an archived match: the Windows
# port of highlight.sh, with the same contract. It writes
# <archivedir>\highlight<sfx>.mp4 / lowlight<sfx>.mp4, which `scry
# --from-archive` embeds under "Highlight" / "Lowlight".
#
# The moments are NOT chosen here. The tick's clip pass hands this script each
# perspective's journaled clip windows, one spec argument per perspective:
#
#   <suffix>|<hl_seek>,<hl_dur>|<ll_seek>,<ll_dur>|<champion>
#
# where <suffix> selects that player's output names (e.g. `-Faker_KR1`), a side
# with no pick is an empty field, and <champion> locks the replay camera on
# that perspective (empty falls back to the auto-director). The script loads
# the replay ONCE, seeks each window, records it, and transcodes the game's
# native webm to a Discord-friendly mp4.
#
# Requires the League CLIENT running + logged in on the match's region/patch,
# EnableReplayApi=1 in game.cfg, curl.exe (bundled with Windows 10 1803+) and
# ffmpeg on PATH. The client lockfile is read from SCRY_LOL_LOCKFILE (the tick
# passes its --lol-lockfile through) or the stock install path.
#
# UNTESTED: ported from the macOS script without a Windows machine to run it
# on. The replay and client APIs are the same on both platforms; what differs
# is the lockfile location, the game process name, and how the display is
# held awake.
#
# Usage: powershell -File scripts\highlight.ps1 <archivedir> <spec>...

param(
    [Parameter(Mandatory = $true, Position = 0)][string]$Dir,
    [Parameter(Position = 1, ValueFromRemainingArguments = $true)][string[]]$Specs
)

$ErrorActionPreference = 'Continue'
if (-not $Specs -or $Specs.Count -eq 0) { Write-Output 'no clip specs given'; exit 0 }

# The game resolves the record `path` from ITS own cwd, so it must be absolute.
$Dir = (Resolve-Path -LiteralPath $Dir).Path
$gid = Split-Path -Leaf $Dir

# PREROLL is just buffer: we seek a couple seconds before the clip start and
# let the replay stream in before recording.
$PREROLL = 3

$lockfile = if ($env:SCRY_LOL_LOCKFILE) { $env:SCRY_LOL_LOCKFILE } else { 'C:\Riot Games\League of Legends\lockfile' }
if (-not (Test-Path -LiteralPath $lockfile)) {
    Write-Output 'no League client lockfile - is it running/logged in?'
    exit 1
}
$fields = (Get-Content -LiteralPath $lockfile -Raw).Trim().Split(':')
$port = $fields[2]
$pass = $fields[3]
$lcu = "https://127.0.0.1:$port"
$rp = 'https://127.0.0.1:2999/replay'

# curl.exe rather than Invoke-RestMethod: it skips the self-signed certs on
# both local APIs the same way on Windows PowerShell 5.1 and PowerShell 7.
function Invoke-Lcu([string]$Method, [string]$Path, [string]$Body = '', [int]$Timeout = 5) {
    $curlArgs = @('-sk', '--max-time', "$Timeout", '-u', "riot:$pass", '-X', $Method, "$lcu$Path")
    if ($Body) { $curlArgs += @('-H', 'Content-Type: application/json', '-d', $Body) }
    & curl.exe @curlArgs 2>$null
}

function Invoke-Replay([string]$Method, [string]$Path, [string]$Body = '', [int]$Timeout = 5) {
    $curlArgs = @('-sk', '--max-time', "$Timeout", '-X', $Method, "$rp$Path")
    if ($Body) { $curlArgs += @('-H', 'Content-Type: application/json', '-d', $Body) }
    & curl.exe @curlArgs 2>$null
}

function Get-JsonField([string]$Json, [string]$Name) {
    try {
        $value = ($Json | ConvertFrom-Json).$Name
        if ($null -eq $value) { '' } else { "$value" }
    } catch { '' }
}

# SAFETY: recording kills the game process, which is also a LIVE match. If a
# game / champ select is in progress, bail so we never kill a real game the
# user is playing. The clip is skipped; the post still goes out.
$phase = Invoke-Lcu 'GET' '/lol-gameflow/v1/gameflow-phase'
if ($phase -match 'InProgress|ChampSelect|Matchmaking|ReadyCheck|GameStart|Reconnect') {
    Write-Output "live game in progress ($phase) - skipping clip recording"
    exit 0
}

# The game process needs an awake display to initialize its renderer; hold the
# system and display awake for the whole recording pass (the caffeinate
# equivalent) and release it on the way out.
Add-Type -Namespace Scry -Name Power -MemberDefinition @'
[DllImport("kernel32.dll")] public static extern uint SetThreadExecutionState(uint esFlags);
'@
# Decimal literals: a hex literal with the top bit set parses as a negative
# Int32 and will not convert to the uint the API takes.
$ES_CONTINUOUS = [uint32]2147483648
$ES_SYSTEM_REQUIRED = [uint32]1
$ES_DISPLAY_REQUIRED = [uint32]2
[void][Scry.Power]::SetThreadExecutionState($ES_CONTINUOUS -bor $ES_SYSTEM_REQUIRED -bor $ES_DISPLAY_REQUIRED)

# The replay runs as the game executable ("League of Legends.exe"); the client
# ("LeagueClient*.exe") stays up because it serves the LCU we download through.
function Stop-Game { Stop-Process -Name 'League of Legends' -Force -ErrorAction SilentlyContinue }

function Get-ReplayState { Get-JsonField (Invoke-Lcu 'GET' "/lol-replays/v1/metadata/$gid") 'state' }

function Test-Playback {
    (& curl.exe -sk --max-time 3 -o NUL -w '%{http_code}' "$rp/playback" 2>$null) -eq '200'
}

# Load THIS game's replay (a stale one may be up). The .rofl download verifies
# asynchronously (metadata state: checking -> watch); launching while still
# "checking" no-ops the watch and the replay never comes up. So: download,
# wait for "watch", launch, poll playback.
function Start-Replay {
    Stop-Game
    Start-Sleep -Seconds 4
    [void](Invoke-Lcu 'POST' "/lol-replays/v1/rofls/$gid/download/graceful" '{}' 10)
    for ($i = 0; $i -lt 20; $i++) {
        if ((Get-ReplayState) -eq 'watch') { break }
        Start-Sleep -Seconds 3
    }
    [void](Invoke-Lcu 'POST' "/lol-replays/v1/rofls/$gid/watch" '{}' 10)
    for ($i = 0; $i -lt 15; $i++) {
        if (Test-Playback) { return $true }
        Start-Sleep -Seconds 3
    }
    return (Test-Playback)
}

# Lock-Camera <champion>: attach the camera to a champion's player. Right after
# the replay comes up the render API accepts the POST but resolves the
# selection to '' (free camera at the fountain), so retry until the response
# echoes a non-empty selectionName. On persistent failure fall through
# unlocked; the auto-director is worse, not fatal.
function Lock-Camera([string]$Champ) {
    for ($i = 0; $i -lt 10; $i++) {
        $body = '{"selectionName":"' + $Champ + '","cameraAttached":true}'
        $sel = Get-JsonField (Invoke-Replay 'POST' '/render' $body) 'selectionName'
        if ($sel) { Write-Output "  camera locked on $Champ ($sel)"; return }
        Start-Sleep -Seconds 2
    }
    Write-Output "  camera lock on $Champ never resolved; recording unlocked"
}

# Save-Clip <start_seconds> <duration_seconds> <out.mp4>: live record button,
# no offline render. Seeks PREROLL before the window so it streams in, then
# records exactly <duration> seconds of the play.
function Save-Clip([int]$Start, [int]$Duration, [string]$Out) {
    $seek = [Math]::Max($Start - $PREROLL, 0)
    $raw = Join-Path $Dir "clip-raw$sfx.webm"
    Remove-Item -LiteralPath $raw -Force -ErrorAction SilentlyContinue
    [void](Invoke-Replay 'POST' '/playback' ('{"time":' + $seek + '.0,"speed":1.0,"paused":false}'))
    Start-Sleep -Seconds $PREROLL
    $rawJson = $raw -replace '\\', '\\'
    [void](Invoke-Replay 'POST' '/recording' ('{"recording":true,"codec":"webm","path":"' + $rawJson + '"}') 6)
    Start-Sleep -Seconds $Duration
    [void](Invoke-Replay 'POST' '/recording' '{"recording":false}' 6)
    Start-Sleep -Seconds 4
    if (-not (Test-Path -LiteralPath $raw) -or (Get-Item -LiteralPath $raw).Length -eq 0) {
        Write-Output "  recording produced no file for $Out"
        return $false
    }
    # crf 28 keeps a ~30s teamfight clip under Discord's 10MB attachment limit.
    & ffmpeg -hide_banner -loglevel error -y -i $raw -vf 'scale=1280:-2,format=yuv420p' `
        -c:v libx264 -crf 28 -preset medium -c:a aac -b:a 96k -movflags +faststart $Out
    Remove-Item -LiteralPath $raw -Force -ErrorAction SilentlyContinue
    $megabytes = [Math]::Round((Get-Item -LiteralPath $Out).Length / 1MB, 1)
    Write-Output "  $Out (${megabytes}M)"
    return $true
}

# Save-Window <seek,dur> <out.mp4>: one side's window, skipped when empty.
function Save-Window([string]$Window, [string]$Out) {
    if (-not $Window) { return }
    $seek, $dur = $Window.Split(',')
    Write-Output "  window: start=${seek}s dur=${dur}s"
    [void](Save-Clip ([int]$seek) ([int]$dur) $Out)
}

try {
    if (-not (Start-Replay)) {
        Write-Output "replay didn't come up; retrying load once"
        if (-not (Start-Replay)) { Write-Output 'replay API never came up'; exit 1 }
    }

    # Clean shot: HUD off, full vision.
    [void](Invoke-Replay 'POST' '/render' '{"interfaceAll":false,"fogOfWar":false}')

    # One loaded replay serves every perspective: seeks are cheap, loads aren't.
    foreach ($spec in $Specs) {
        $parts = $spec.Split('|')
        $sfx = $parts[0]
        $hl = if ($parts.Count -gt 1) { $parts[1] } else { '' }
        $ll = if ($parts.Count -gt 2) { $parts[2] } else { '' }
        $champ = if ($parts.Count -gt 3) { $parts[3] } else { '' }
        if ($champ) { Lock-Camera $champ }
        if ($hl) { Write-Output "recording highlight$sfx"; Save-Window $hl (Join-Path $Dir "highlight$sfx.mp4") }
        if ($ll) { Write-Output "recording lowlight$sfx"; Save-Window $ll (Join-Path $Dir "lowlight$sfx.mp4") }
    }

    # Close the replay game window (leave the client up).
    Stop-Game
    Write-Output 'closed replay'
} finally {
    [void][Scry.Power]::SetThreadExecutionState($ES_CONTINUOUS)
}
