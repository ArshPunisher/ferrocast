# Windows frame pacing: how many frames a built Ferrocast draws late at 1080p60 with a colour and
# a 1080p clip on air while recording, idle, with the CPU busy (burners on all but two threads,
# as a game would) and minimized while busy (where Windows throttles an app). The numbers to
# compare before and after a change to the frame loop (WINDOWS-HANDOFF.md §5).
#
#   tools/windows-pace.ps1 -Exe target\release\ferrocast.exe -Out C:\fc-pace -Secs 60
#
# The exe must write its token file (a debug build, or a release build with
# --config 'profile.release.package.ferrocast-desktop.debug-assertions=true'). Close any other
# Ferrocast first. Everything goes under -Out (a test home: never the owner's data).
param(
  [Parameter(Mandatory)] [string]$Exe,
  [string]$Out = 'pace',
  [int]$Secs = 60,
  # Comma-separated (powershell -File passes a list as one word).
  [string]$Modes = 'idle,busy,minimized'
)
$ErrorActionPreference = 'Stop'
$Out = (New-Item -ItemType Directory -Force $Out).FullName
$home_ = Join-Path $Out 'home'
$rec = Join-Path $Out 'recordings'
$tokenFile = Join-Path $Out 'token.txt'
New-Item -ItemType Directory -Force $home_, $rec | Out-Null
Remove-Item $tokenFile -ErrorAction SilentlyContinue
Add-Type @'
using System; using System.Runtime.InteropServices;
public static class PaceWnd { [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd); }
'@

$env:FERROCAST_HOME = $home_
$env:FERROCAST_DEBUG_TOKEN_FILE = $tokenFile
$app = Start-Process (Resolve-Path $Exe) -PassThru
for ($i = 0; $i -lt 90 -and -not (Test-Path $tokenFile); $i++) { Start-Sleep 1 }
if (-not (Test-Path $tokenFile)) { throw 'the app wrote no token file' }
$base, $token = (Get-Content $tokenFile -Raw).Trim().Split(' ')
function Api($method, $path, $body) {
  $a = @{ Method = $method; Uri = "$base/api/v1/studio/engine$path"; Headers = @{ Authorization = "Bearer $token" }
    ContentType = 'application/json'; UseBasicParsing = $true; TimeoutSec = 60 }
  # -InputObject: a pipe would unwrap a one-item list.
  if ($null -ne $body) { $a.Body = ConvertTo-Json -InputObject $body -Depth 6 -Compress }
  $r = Invoke-WebRequest @a
  if ($r.Content) { $r.Content | ConvertFrom-Json } else { $null }
}

try {
  # Set before the Studio starts: the canvas is fixed for the engine's life.
  $set = Api GET '/settings'
  $set.record_folder = $rec; $set.fps = 60; $set.width = 1920; $set.height = 1080; $set.record_quality = 'stream'
  $set.PSObject.Properties.Remove('record_folder_default')
  Api PUT '/settings' $set | Out-Null
  Api POST '/start' @{} | Out-Null

  # A 1080p30 clip, made with the FFmpeg the app fetches into its home at start.
  $ffDir = Join-Path $home_ 'tools'
  $ff = $null
  for ($i = 0; $i -lt 180 -and -not $ff; $i++) {
    $ff = Get-ChildItem $ffDir -Recurse -Filter ffmpeg.exe -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $ff) { Start-Sleep 1 }
  }
  if (-not $ff) { throw 'FFmpeg was not fetched' }
  $clip = Join-Path $Out 'clip1080.mp4'
  if (-not (Test-Path $clip)) {
    $ErrorActionPreference = 'Continue'
    & $ff.FullName -hide_banner -loglevel error -y -f lavfi -i testsrc2=size=1920x1080:rate=30 -f lavfi -i sine=frequency=440:sample_rate=48000 -t 40 -c:v libx264 -preset veryfast -pix_fmt yuv420p -c:a aac $clip 2>&1 | Out-Null
    $ErrorActionPreference = 'Stop'
  }
  $scene = (Api GET '/scenes').current
  $sc = [uri]::EscapeDataString($scene)
  Api POST "/scenes/$sc/items" @{ kind = 'color_source_v3'; name = 'Back'; settings = @{ color = 4281545523; width = 1920; height = 1080 } } | Out-Null
  Api POST "/scenes/$sc/items" @{ kind = 'ffmpeg_source'; name = 'Clip'; settings = @{ local_file = $clip; looping = $true }; fit = $true } | Out-Null
  # The engine starts drawing when a picture is first asked for.
  $shot = Api POST "/sources/$sc/screenshot"
  if ($shot.file) { Remove-Item $shot.file -ErrorAction SilentlyContinue }
  Start-Sleep 5
  $c = (Api GET '/stats').canvas
  Write-Host "canvas $($c.width)x$($c.height) at $($c.fps) fps"
  Api POST '/record' @{ on = $true } | Out-Null
  Start-Sleep 3

  $burn = @()
  $results = [ordered]@{}
  foreach ($mode in ($Modes -split ',' | ForEach-Object { $_.Trim() } | Where-Object { $_ })) {
    if ($mode -ne 'idle' -and -not $burn) {
      $n = [Math]::Max(1, [Environment]::ProcessorCount - 2)
      $burn = 1..$n | ForEach-Object { Start-Process powershell -WindowStyle Hidden -PassThru -ArgumentList '-NoProfile', '-Command', 'while ($true) { [Math]::Sqrt(2) | Out-Null }' }
      Start-Sleep 2
    }
    $hwnd = (Get-Process -Id $app.Id).MainWindowHandle
    [PaceWnd]::ShowWindow($hwnd, $(if ($mode -eq 'minimized') { 6 } else { 9 })) | Out-Null
    Start-Sleep 2
    $a = Api GET '/stats'
    Start-Sleep $Secs
    $b = Api GET '/stats'
    $made = $b.engine.frames - $a.engine.frames
    $late = $b.engine.lagged - $a.engine.lagged
    $results[$mode] = [ordered]@{ frames = $made; late = $late; late_percent = [Math]::Round(100 * $late / [Math]::Max(1, $made + $late), 2)
      recording_dropped = $b.record.dropped - $a.record.dropped; cpu = $b.engine.cpu; encoder = $b.record.encoder }
    Write-Host ("{0,-10} frames {1}, late {2} ({3}%), recording dropped {4}, encoder {5}" -f $mode, $made, $late, $results[$mode].late_percent, $results[$mode].recording_dropped, $b.record.encoder)
  }
  $results | ConvertTo-Json -Depth 4 | Set-Content (Join-Path $Out 'pace.json')
} finally {
  $burn | ForEach-Object { Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue }
  if (-not $app.HasExited) { Stop-Process -Id $app.Id -Force }
  Get-ChildItem $rec -ErrorAction SilentlyContinue | Remove-Item -Force -ErrorAction SilentlyContinue
}
