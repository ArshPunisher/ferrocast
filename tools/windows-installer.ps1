# Windows installer test: installs a published Ferrocast setup (NSIS, per user) the way people
# do, silently, then checks what it left: the program, its icon, Start menu shortcut, the
# Apps & features entry, that the app starts and listens; then an update over it, and the
# uninstaller (nothing of the program left, the user's data kept). Used by
# .github/workflows/windows-installer.yml; runnable on any Windows PC (it installs for the
# signed-in user and removes it again).
#
#   tools/windows-installer.ps1 -Setup Ferrocast_3.3.0_x64-setup.exe [-Older Ferrocast_3.2.1_x64-setup.exe] -Out inst
param(
  [Parameter(Mandatory)] [string]$Setup,
  # An older setup installed first, so the new one is tested as an update.
  [string]$Older = '',
  [string]$Out = 'inst'
)
$ErrorActionPreference = 'Stop'
$Out = (New-Item -ItemType Directory -Force $Out).FullName
$result = [ordered]@{ ok = $true; checks = [ordered]@{} }
function Check($name, [bool]$ok, $detail) {
  $result.checks[$name] = [ordered]@{ ok = $ok; detail = "$detail" }
  if (-not $ok) { $result.ok = $false }
  Write-Host ("{0} {1}: {2}" -f ($(if ($ok) { 'PASS' } else { 'FAIL' })), $name, $detail)
}
Add-Type -AssemblyName System.Drawing

$uninstallKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Ferrocast'
function Entry { Get-ItemProperty $uninstallKey -ErrorAction SilentlyContinue }
function Install($exe, $label) {
  $t = [Diagnostics.Stopwatch]::StartNew()
  $p = Start-Process -FilePath $exe -ArgumentList '/S' -PassThru -Wait
  Check "$label exits cleanly" ($p.ExitCode -eq 0) "exit $($p.ExitCode) after $([int]$t.Elapsed.TotalSeconds) s"
}
function SaveIcon($file, $name) {
  try {
    $icon = [System.Drawing.Icon]::ExtractAssociatedIcon($file)
    $png = Join-Path $Out "$name.png"
    $icon.ToBitmap().Save($png, [System.Drawing.Imaging.ImageFormat]::Png)
    return $png
  } catch { return $null }
}
# Every pixel of a saved icon, to compare two (ExtractAssociatedIcon always gives some icon).
function Pixels($png) {
  $b = New-Object System.Drawing.Bitmap $png
  try { (0..($b.Height - 1) | ForEach-Object { $y = $_; 0..($b.Width - 1) | ForEach-Object { $b.GetPixel($_, $y).ToArgb() } }) -join ',' }
  finally { $b.Dispose() }
}

# The setup's own icon (owner, 4 Nov: it showed NSIS's disc).
$setupIcon = SaveIcon (Resolve-Path $Setup) 'setup-icon'
Check 'setup has an icon' ($null -ne $setupIcon) "$setupIcon"

if ($Older) {
  Install (Resolve-Path $Older) 'older setup'
  $e = Entry
  Check 'older version installed' ($null -ne $e) "$($e.DisplayVersion)"
}
Install (Resolve-Path $Setup) 'setup'
$e = Entry
Check 'Apps & features entry' ($null -ne $e) "$($e.DisplayName) $($e.DisplayVersion) by $($e.Publisher)"
$dir = if ($e -and $e.InstallLocation) { $e.InstallLocation.Trim('"') } else { Join-Path $env:LOCALAPPDATA 'Ferrocast' }
$exe = Join-Path $dir 'ferrocast.exe'
Check 'program installed' (Test-Path $exe) $exe
Check 'per user (no admin)' ($dir -like "$env:LOCALAPPDATA*") $dir
# The version this setup installs, from its own name (Ferrocast_3.3.0_x64-setup.exe), not from
# what got installed: a setup that left the older version in place must fail here.
$want = if ((Split-Path $Setup -Leaf) -match '_(\d+\.\d+\.\d+[^_]*)_') { $Matches[1] } else { '' }
$have = if (Test-Path $exe) { (Get-Item $exe).VersionInfo.ProductVersion } else { '' }
Check 'program is the setup''s version' ($want -and $have -like "$want*") "want $want, exe $have"
Check 'Apps & features shows the setup''s version' ($want -and $e -and $e.DisplayVersion -eq $want) "want $want, entry $($e.DisplayVersion)"
$entries = @(Get-ChildItem 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall' | Where-Object { (Get-ItemProperty $_.PSPath).DisplayName -like 'Ferrocast*' })
Check 'one Apps & features entry after the update' ($entries.Count -eq 1) "$($entries.Count)"
$cams = @(Get-ChildItem $dir -Filter 'ferrocast_camera*.dll' -ErrorAction SilentlyContinue | ForEach-Object Name)
Check 'camera DLLs beside the program (64 and 32-bit)' ($cams.Count -ge 2) ($cams -join ', ')
$appIcon = if (Test-Path $exe) { SaveIcon $exe 'app-icon' } else { $null }
Check 'program has an icon' ($null -ne $appIcon) "$appIcon"
# The setup shows the program's own icon, not NSIS's default disc (owner's PC test, 4 Nov).
$same = $setupIcon -and $appIcon -and ((Pixels $setupIcon) -eq (Pixels $appIcon))
Check "setup shows Ferrocast's icon" $same 'setup-icon.png and app-icon.png, pixel for pixel'
$lnk = Get-ChildItem "$env:APPDATA\Microsoft\Windows\Start Menu\Programs" -Recurse -Filter 'Ferrocast*.lnk' -ErrorAction SilentlyContinue | Select-Object -First 1
Check 'Start menu shortcut' ($null -ne $lnk) "$($lnk.FullName)"
if ($lnk) {
  $target = (New-Object -ComObject WScript.Shell).CreateShortcut($lnk.FullName).TargetPath
  Check 'shortcut opens the program' ($target -eq $exe) $target
}
Get-ChildItem $dir -Recurse | Select-Object FullName, Length | Out-File (Join-Path $Out 'files.txt')

# It starts from where it was installed (with a test data folder) and listens.
$home_ = Join-Path $Out 'home'
New-Item -ItemType Directory -Force $home_ | Out-Null
$env:FERROCAST_HOME = $home_
$env:RUST_LOG = 'info'
# Something of the user's, to be found unchanged after the uninstall: in the data folder the
# app uses when it isn't told another (main.rs data_dir: %APPDATA%\Ferrocast), which the
# uninstaller could reach; the test's own FERROCAST_HOME it never knew about.
$realData = Join-Path $env:APPDATA 'Ferrocast'
New-Item -ItemType Directory -Force $realData | Out-Null
# A name of its own: never a file of the user's on a real PC (removed after the check).
$sentinel = Join-Path $realData ("installer-test-{0}.txt" -f [guid]::NewGuid())
$stamp = [guid]::NewGuid().ToString()
Set-Content -Path $sentinel -Value $stamp -NoNewline
if (-not (Test-Path $exe)) {
  Check 'installed app starts and listens' $false 'no program to start'
} else {
  $app = Start-Process -FilePath $exe -PassThru -RedirectStandardOutput (Join-Path $Out 'app.out.log') -RedirectStandardError (Join-Path $Out 'app.err.log')
  $listening = $false
  for ($i = 0; $i -lt 60 -and -not $listening; $i++) {
    Start-Sleep 1
    $listening = [bool](Get-NetTCPConnection -State Listen -LocalPort 47820 -ErrorAction SilentlyContinue)
    if ($app.HasExited) { break }
  }
  Check 'installed app starts and listens' ($listening -and -not $app.HasExited) "listening=$listening exited=$($app.HasExited) code=$(if ($app.HasExited) { $app.ExitCode })"
  Start-Sleep 5
  Check 'still running after 5 s more' (-not $app.HasExited) ''
  Stop-Process -Id $app.Id -Force -ErrorAction SilentlyContinue
  Start-Sleep 2
}

# Uninstall: silent, the program and its entry gone, the user's data kept.
$un = Join-Path $dir 'uninstall.exe'
Check 'uninstaller present' (Test-Path $un) $un
if (Test-Path $un) {
  $p = Start-Process -FilePath $un -ArgumentList '/S' -PassThru -Wait
  # NSIS's uninstaller copies itself to %TEMP% and returns at once: wait for the folder to go.
  for ($i = 0; $i -lt 60 -and (Test-Path $exe); $i++) { Start-Sleep 1 }
  Check 'uninstaller exits cleanly' ($p.ExitCode -eq 0) "exit $($p.ExitCode)"
  Check 'program removed' (-not (Test-Path $exe)) $exe
  $left = @(Get-ChildItem $dir -Recurse -ErrorAction SilentlyContinue | ForEach-Object FullName)
  Check 'nothing left in the program folder' ($left.Count -eq 0) ($left -join '; ')
  Check 'Apps & features entry removed' ($null -eq (Entry)) ''
  $lnk2 = Get-ChildItem "$env:APPDATA\Microsoft\Windows\Start Menu\Programs" -Recurse -Filter 'Ferrocast*.lnk' -ErrorAction SilentlyContinue
  Check 'Start menu shortcut removed' ($null -eq $lnk2) "$($lnk2.FullName)"
  $cam = Test-Path 'HKCU:\Software\Classes\CLSID\{9E5CC468-3A73-45C7-AE12-0E584A9A4E36}'
  Check 'camera registration removed' (-not $cam) ''
  $kept = (Test-Path $sentinel) -and ((Get-Content -Raw $sentinel) -eq $stamp)
  Check "the user's data kept" $kept $sentinel
  Remove-Item $sentinel -ErrorAction SilentlyContinue
}

$result | ConvertTo-Json -Depth 5 | Out-File (Join-Path $Out 'results.json')
if (-not $result.ok) { Write-Host 'INSTALLER TEST FAILED'; exit 1 }
Write-Host 'INSTALLER TEST PASSED'
