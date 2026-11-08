# Windows smoke test: starts a built Ferrocast, drives the Studio through the local API (a
# display, an Edge window playing a tone with its sound, desktop sound and a mic), records
# for a while and checks the app lived, the recording has picture and sound, and how hard the
# machine worked. Used by .github/workflows/windows-smoke.yml and runnable on any Windows PC.
#
#   tools/windows-smoke.ps1 -Exe target\release\ferrocast.exe -Out smoke
#
# The exe must write its token file (a debug build, or a release build with
# --config 'profile.release.package.ferrocast-desktop.debug-assertions=true').
# Close any other Ferrocast first: the app is single-instance. Everything it writes (home,
# recordings, log, screenshots, results.json, a crash dump if it dies) goes under -Out.
param(
  [Parameter(Mandatory)] [string]$Exe,
  [string]$Out = 'smoke',
  [int]$Seconds = 20,
  [int]$Fps = 30,
  # The share of $Fps the engine must reach: 0.9 on a real PC. GitHub's runner has no
  # graphics card (WARP draws on 4 CPU cores, ~14 of 30 fps), so CI asks for less there.
  [double]$MinShare = 0.9,
  # A minidump on a native crash (Sysinternals ProcDump, downloaded when missing).
  [string]$ProcDump = '',
  # Instead: the app faults on purpose (FERROCAST_TEST_CRASH) and its own crash report must
  # appear in <home>\crashes. No ProcDump then: Windows skips the app's handler under a debugger.
  [switch]$Crash
)
$ErrorActionPreference = 'Stop'
$Out = (New-Item -ItemType Directory -Force $Out).FullName
$home_ = Join-Path $Out 'home'
$rec = Join-Path $Out 'recordings'
$tokenFile = Join-Path $Out 'token.txt'
New-Item -ItemType Directory -Force $home_, $rec | Out-Null
Remove-Item $tokenFile -ErrorAction SilentlyContinue
$result = [ordered]@{ ok = $false; checks = [ordered]@{}; notes = @() }
function Check($name, [bool]$ok, $detail) {
  $result.checks[$name] = [ordered]@{ ok = $ok; detail = "$detail" }
  Write-Host ("{0} {1}: {2}" -f ($(if ($ok) { 'PASS' } else { 'FAIL' })), $name, $detail)
}
function Note($text) { $result.notes += $text; Write-Host "NOTE $text" }

Add-Type -AssemblyName System.Drawing, System.Windows.Forms
function Shot($name) {
  try {
    $b = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
    $bmp = New-Object System.Drawing.Bitmap $b.Width, $b.Height
    [System.Drawing.Graphics]::FromImage($bmp).CopyFromScreen($b.X, $b.Y, 0, 0, $bmp.Size)
    $bmp.Save((Join-Path $Out "$name.png"))
  } catch { Note "no screenshot ${name}: $($_.Exception.Message)" }
}

if ($Crash) {
  $env:FERROCAST_HOME = $home_
  $env:FERROCAST_DEBUG_TOKEN_FILE = $tokenFile
  $env:FERROCAST_TEST_CRASH = '1'
  $app = Start-Process (Resolve-Path $Exe) -PassThru
  $gone = $app.WaitForExit(90000)
  Remove-Item Env:FERROCAST_TEST_CRASH
  Check 'app faults on purpose' $gone $(if ($gone) { 'exit code 0x{0:X8}' -f $app.ExitCode } else { 'still running after 90 s' })
  if (-not $gone) { Stop-Process -Id $app.Id -Force }
  $dmp = Get-ChildItem (Join-Path $home_ 'crashes') -Filter *.dmp -ErrorAction SilentlyContinue
  Check 'crash report written' ([bool]$dmp) $(if ($dmp) { "$($dmp.Name), $([math]::Round($dmp.Length / 1MB, 1)) MB" } else { 'none in crashes' })
  # The next start says so in the log.
  Remove-Item $tokenFile -ErrorAction SilentlyContinue
  $app = Start-Process (Resolve-Path $Exe) -PassThru
  for ($i = 0; $i -lt 60 -and -not (Test-Path $tokenFile); $i++) { Start-Sleep 1 }
  Stop-Process -Id $app.Id -Force -ErrorAction SilentlyContinue
  $said = Select-String -Path (Join-Path $home_ 'logs\ferrocast.log') -Pattern 'a crash report was saved' -ErrorAction SilentlyContinue
  Check 'next start logs it' ([bool]$said) $(if ($said) { $said[-1].Line.Trim() } else { 'not in the log' })
  Copy-Item (Join-Path $home_ 'logs\*'), (Join-Path $home_ 'crashes\*') $Out -ErrorAction SilentlyContinue
  $result.ok = -not ($result.checks.Values | Where-Object { -not $_.ok })
  $result | ConvertTo-Json -Depth 6 | Set-Content (Join-Path $Out 'results.json')
  Write-Host $(if ($result.ok) { 'CRASH TEST PASSED' } else { 'CRASH TEST FAILED' })
  if (-not $result.ok) { exit 1 }
  exit 0
}

# A page that plays a 440 Hz tone, in its own Edge window (the window source, with its sound).
$tone = Join-Path $Out 'tone.html'
@'
<!doctype html><title>Ferrocast smoke tone</title><body style="background:#2a6;color:#fff;font:48px sans-serif">tone
<script>const c=new AudioContext();const o=c.createOscillator();o.frequency.value=440;const g=c.createGain();g.gain.value=0.2;o.connect(g).connect(c.destination);o.start();</script>
'@ | Set-Content -Encoding utf8 $tone
$edgeExe = @("${env:ProgramFiles(x86)}\Microsoft\Edge\Application\msedge.exe", "$env:ProgramFiles\Microsoft\Edge\Application\msedge.exe") |
  Where-Object { Test-Path $_ } | Select-Object -First 1
$edge = $null
if ($edgeExe) {
  $edge = Start-Process $edgeExe -PassThru -ArgumentList @(
    "--user-data-dir=$(Join-Path $Out 'edge')", '--no-first-run', '--new-window',
    '--autoplay-policy=no-user-gesture-required', "file:///$($tone -replace '\\', '/')")
} else { Note 'no Edge: the window source is skipped' }

# The app, with a crash dump if it dies.
$env:FERROCAST_HOME = $home_
$env:FERROCAST_DEBUG_TOKEN_FILE = $tokenFile
$app = Start-Process (Resolve-Path $Exe) -PassThru
$dump = $null
try {
  if (-not $ProcDump) {
    $ProcDump = Join-Path $Out 'procdump64.exe'
    if (-not (Test-Path $ProcDump)) { Invoke-WebRequest https://live.sysinternals.com/procdump64.exe -OutFile $ProcDump -UseBasicParsing }
  }
  $dumps = New-Item -ItemType Directory -Force (Join-Path $Out 'dumps')
  $dump = Start-Process $ProcDump -PassThru -WindowStyle Hidden -ArgumentList @('-accepteula', '-ma', '-e', "$($app.Id)", $dumps.FullName)
} catch { Note "no crash dumps: $($_.Exception.Message)" }

for ($i = 0; $i -lt 90 -and -not (Test-Path $tokenFile); $i++) { Start-Sleep 1 }
Check 'app starts' (Test-Path $tokenFile) 'it wrote its API token'
if (-not (Test-Path $tokenFile)) { $result | ConvertTo-Json -Depth 5 | Set-Content (Join-Path $Out 'results.json'); exit 1 }
$base, $token = (Get-Content $tokenFile -Raw).Trim().Split(' ')
function Api($method, $path, $body) {
  $a = @{ Method = $method; Uri = "$base/api/v1/studio/engine$path"; Headers = @{ Authorization = "Bearer $token" }
    ContentType = 'application/json'; UseBasicParsing = $true; TimeoutSec = 60 }
  if ($null -ne $body) { $a.Body = ($body | ConvertTo-Json -Depth 6 -Compress) }
  $r = Invoke-WebRequest @a
  if ($r.Content) { $r.Content | ConvertFrom-Json } else { $null }
}
function Try-Api($what, $method, $path, $body) {
  try { $r = Api $method $path $body; Check $what $true 'ok'; $r }
  catch { Check $what $false "$($_.Exception.Message) $($_.ErrorDetails.Message)"; $null }
}

$started = Try-Api 'Studio starts' POST '/start' @{}
if ($started) { $result.engine = $started }
$settings = Api GET '/settings'
$settings.record_folder = $rec
$settings.fps = $Fps
Try-Api 'settings (recordings folder, fps)' PUT '/settings' $settings | Out-Null
$scene = (Api GET '/scenes').current

Try-Api 'add display' POST "/scenes/$([uri]::EscapeDataString($scene))/items" @{ kind = 'monitor_capture'; name = 'Display'; settings = @{ monitor = 0 }; fit = $true } | Out-Null
if ($edge) {
  # The window's id from the source's own list (its title may carry Edge's profile name).
  $win = $null
  for ($i = 0; $i -lt 20 -and -not $win; $i++) {
    $o = (Api GET '/kinds/window_capture/options') | Where-Object key -eq 'window'
    for ($j = 0; $j -lt $o.choices.Count; $j++) { if ($o.choices[$j] -like '*smoke tone*') { $win = $o.values[$j] } }
    if (-not $win) { Start-Sleep 1 }
  }
  if ($win) {
    Try-Api 'add Edge window (with its sound)' POST "/scenes/$([uri]::EscapeDataString($scene))/items" @{ kind = 'window_capture'; name = 'Edge'; settings = @{ window = $win } } | Out-Null
  } else { Check 'add Edge window (with its sound)' $false 'the Edge window is not in the list' }
}
Try-Api 'desktop sound' PUT '/channels/1' @{ kind = 'wasapi_output_capture'; name = 'Desktop Audio'; settings = @{ device_id = 'default' } } | Out-Null
try { Api PUT '/channels/3' @{ kind = 'wasapi_input_capture'; name = 'Mic'; settings = @{ device_id = 'default' } } | Out-Null }
catch { Note "no microphone: $($_.ErrorDetails.Message)" }
Start-Sleep 3
Shot 'sources'

# A fresh home has no FFmpeg yet: the app fetches it in the background at start, and the
# recording is refused until it's there.
$ffDir = Join-Path $home_ 'tools'
for ($i = 0; $i -lt 180 -and -not (Get-ChildItem $ffDir -Recurse -Filter ffmpeg.exe -ErrorAction SilentlyContinue); $i++) { Start-Sleep 1 }
Check 'FFmpeg fetched' ([bool](Get-ChildItem $ffDir -Recurse -Filter ffmpeg.exe -ErrorAction SilentlyContinue)) "under $ffDir"
Start-Sleep 2

# Listing cameras (FFmpeg asks DirectShow) is off the window's own thread: the live picture's
# place still changes at once meanwhile (owner, 8 Nov: it sat over the Add dialog for seconds).
$list = Start-Job -ArgumentList $base, $token -ScriptBlock {
  param($b, $t)
  $sw = [Diagnostics.Stopwatch]::StartNew()
  Invoke-WebRequest -Uri "$b/api/v1/studio/engine/kinds/dshow_input/options" -Headers @{ Authorization = "Bearer $t" } -UseBasicParsing | Out-Null
  $sw.ElapsedMilliseconds
}
Start-Sleep -Milliseconds 150
$sw = [Diagnostics.Stopwatch]::StartNew()
try { Api PUT '/preview' @{ rect = @{ x = 0; y = 0; w = 640; h = 360; vh = 800 } } | Out-Null } catch { }
$placeMs = $sw.ElapsedMilliseconds
$listMs = Receive-Job $list -Wait -AutoRemoveJob
Check 'preview placed while cameras are listed' ($placeMs -lt 500) "placed in $placeMs ms; the camera list took $listMs ms"
$sw = [Diagnostics.Stopwatch]::StartNew()
Api GET '/kinds/dshow_input/options' | Out-Null
Note "the camera list again: $($sw.ElapsedMilliseconds) ms"
try { Api PUT '/preview' @{ rect = $null } | Out-Null } catch { }

# Record, sampling the app's CPU (Windows' view) and the engine's own numbers each second.
$r = Try-Api 'recording starts' POST '/record' @{ on = $true }
$cpu = @(); $fpsSeen = @(); $frameMs = @()
$cores = [Environment]::ProcessorCount
$p = Get-Process -Id $app.Id
$last = $p.TotalProcessorTime; $t0 = Get-Date
for ($i = 0; $i -lt $Seconds; $i++) {
  Start-Sleep 1
  if ($app.HasExited) { break }
  $p.Refresh(); $now = Get-Date
  $cpu += 100 * ($p.TotalProcessorTime - $last).TotalSeconds / ($now - $t0).TotalSeconds / $cores
  $last = $p.TotalProcessorTime; $t0 = $now
  try { $s = Api GET '/stats'; $fpsSeen += $s.engine.fps; $frameMs += $s.engine.frame_ms } catch { }
  if ($i -eq [int]($Seconds / 2)) { Shot 'recording' }
}
$stats = $null
try { $stats = Api GET '/stats' } catch { }
$stopped = $null
if (-not $app.HasExited) { $stopped = Try-Api 'recording stops' POST '/record' @{ on = $false } }

Check 'app still running' (-not $app.HasExited) $(if ($app.HasExited) { "exit code $($app.ExitCode)" } else { "after $Seconds s of recording" })
$avg = { param($a) if ($a.Count) { [math]::Round(($a | Measure-Object -Average).Average, 1) } else { $null } }
$result.numbers = [ordered]@{
  cpu_percent_of_machine = & $avg $cpu
  engine_fps = & $avg $fpsSeen
  frame_ms = & $avg $frameMs
  lagged = $stats.engine.lagged
  record_frames = $stats.record.frames
  record_dropped = $stats.record.dropped
  canvas = $stats.canvas
  cores = $cores
}
Write-Host ("numbers: " + ($result.numbers | ConvertTo-Json -Compress))
if ($null -ne $result.numbers.engine_fps) { Check 'frame rate' ($result.numbers.engine_fps -ge $MinShare * $Fps) "$($result.numbers.engine_fps) of $Fps fps (needs $([math]::Round($MinShare * $Fps, 1)))" }

# The file: picture and sound in it (FFmpeg's own reading, the copy the app fetched).
$file = Get-ChildItem $rec -File -ErrorAction SilentlyContinue | Sort-Object LastWriteTime | Select-Object -Last 1
Check 'recording written' ($null -ne $file -and $file.Length -gt 100KB) $(if ($file) { "$($file.Name), $([math]::Round($file.Length / 1MB, 1)) MB" } else { 'no file' })
$ff = Get-ChildItem (Join-Path $home_ 'tools') -Recurse -Filter ffmpeg.exe -ErrorAction SilentlyContinue | Select-Object -First 1
if ($file -and $ff) {
  # FFmpeg reports on stderr, which Windows PowerShell 5.1 turns into errors under Stop.
  $ErrorActionPreference = 'Continue'
  $info = (& $ff.FullName -hide_banner -i $file.FullName 2>&1 | Out-String)
  $info | Set-Content (Join-Path $Out 'recording-info.txt')
  Check 'recording has picture' ($info -match 'Stream #\S+.*Video:') (($info -split "`n" | Where-Object { $_ -match 'Video:' }) -join ' ').Trim()
  Check 'recording has sound' ($info -match 'Stream #\S+.*Audio:') (($info -split "`n" | Where-Object { $_ -match 'Audio:' }) -join ' ').Trim()
  # How loud: the tone (or anything) should be there, not silence.
  $vol = (& $ff.FullName -hide_banner -i $file.FullName -af volumedetect -vn -f null - 2>&1 | Out-String)
  if ($vol -match 'max_volume: (-?[\d.]+) dB') { $result.numbers.max_volume_db = [double]$Matches[1]; Write-Host "max volume $($Matches[1]) dB" }
  $ErrorActionPreference = 'Stop'
} elseif ($file) { Note 'no FFmpeg in the app home: the file was not read' }

# Leave everything to look at, then close.
Copy-Item (Join-Path $home_ 'logs\*') $Out -ErrorAction SilentlyContinue
if (-not $app.HasExited) { Stop-Process -Id $app.Id -Force }
# Edge's first process hands the window to another and exits: close them by their profile.
$edgeDir = Join-Path $Out 'edge'
Get-CimInstance Win32_Process -Filter "Name='msedge.exe'" | Where-Object { $_.CommandLine -like "*$edgeDir*" } |
  ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
if ($dump -and -not $dump.HasExited) { Stop-Process -Id $dump.Id -Force -ErrorAction SilentlyContinue }
$crash = Get-ChildItem (Join-Path $Out 'dumps') -Filter *.dmp -ErrorAction SilentlyContinue
if ($crash) { Check 'no crash dump' $false ($crash.Name -join ', ') }
$result.ok = -not ($result.checks.Values | Where-Object { -not $_.ok })
$result | ConvertTo-Json -Depth 6 | Set-Content (Join-Path $Out 'results.json')
Write-Host $(if ($result.ok) { 'SMOKE PASSED' } else { 'SMOKE FAILED' })
if (-not $result.ok) { exit 1 }
