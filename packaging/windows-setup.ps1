# Replaces the Windows installer Tauri built (NSIS) with Ferrocast's own setup program
# (apps/setup), then installs, updates and uninstalls it on this machine as a test.
#
#   packaging/windows-setup.ps1 -Bundle target/release/bundle
#
# The app's files are taken from a silent install of Tauri's installer, so the setup carries
# exactly what that installer would have put on a PC (resources, helper programs, and the
# program marked as an NSIS install, which keeps the updater asking for the Windows entry).
# The setup keeps the NSIS file's name, so the update feed and the release don't change.
# With TAURI_SIGNING_PRIVATE_KEY set, the new setup is signed for the updater (.sig).
param([Parameter(Mandatory = $true)][string]$Bundle)
$ErrorActionPreference = 'Stop'

function Run($exe, [string[]]$argv) {
  $p = Start-Process -FilePath $exe -ArgumentList $argv -Wait -PassThru -NoNewWindow
  return $p.ExitCode
}
function Fail($msg) {
  Write-Host '--- %TEMP%\ferrocast-setup.log ---'
  Get-Content "$env:TEMP\ferrocast-setup.log" -ErrorAction SilentlyContinue | Select-Object -Last 40
  Write-Host "::error::$msg"
  exit 1
}

$nsis = Get-ChildItem "$Bundle/nsis/*-setup.exe" | Select-Object -First 1
if (-not $nsis) { Fail "no NSIS installer in $Bundle/nsis" }
$version = [regex]::Match($nsis.Name, '_(\d+\.\d+\.\d+[^_]*)_').Groups[1].Value
if (-not $version) { Fail "can't read the version from $($nsis.Name)" }
Write-Host "Packing $($nsis.Name) (version $version)"

# 1. What the old installer installs.
$stage = Join-Path $env:RUNNER_TEMP 'fc-stage'
Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue
if ((Run $nsis.FullName @('/S', "/D=$stage")) -ne 0) { Fail 'the NSIS installer failed to stage the app' }
$uninstKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Ferrocast'
$main = (Get-ItemProperty $uninstKey).MainBinaryName
if (-not (Test-Path (Join-Path $stage $main))) { Fail "staged app has no $main" }
Remove-Item -Force (Join-Path $stage 'uninstall.exe')
Write-Host "Staged $((Get-ChildItem -Recurse -File $stage).Count) files; program $main"

# 2. The setup program, carrying them.
cargo build --release --locked -p ferrocast-setup
if ($LASTEXITCODE -ne 0) { Fail 'building the setup failed' }
$setup = (Resolve-Path 'target/release/ferrocast-setup.exe').Path
$out = Join-Path $env:RUNNER_TEMP $nsis.Name
if ((Run $setup @('pack', "`"$stage`"", '--main', $main, '--version', $version, '--out', "`"$out`"")) -ne 0) { Fail 'packing failed' }
Write-Host ("NSIS installer {0:N1} MB, new setup {1:N1} MB" -f ($nsis.Length / 1MB), ((Get-Item $out).Length / 1MB))
Remove-Item -Force "$($nsis.FullName).sig" -ErrorAction SilentlyContinue
Move-Item -Force $out $nsis.FullName
if ($env:TAURI_SIGNING_PRIVATE_KEY) {
  cargo tauri signer sign $nsis.FullName
  if ($LASTEXITCODE -ne 0 -or -not (Test-Path "$($nsis.FullName).sig")) { Fail 'signing the setup for the updater failed' }
}

# 3. Test: install, update over it, uninstall. The staging install is removed first.
Remove-Item -Recurse -Force $stage
Remove-Item -Recurse -Force $uninstKey -ErrorAction SilentlyContinue
$dir = Join-Path $env:RUNNER_TEMP 'fc-test\Ferrocast'
$lnk = Join-Path ([Environment]::GetFolderPath('Programs')) 'Ferrocast.lnk'
Remove-Item -Force $lnk -ErrorAction SilentlyContinue

if ((Run $nsis.FullName @('/S', "/D=$dir")) -ne 0) { Fail 'silent install failed (see %TEMP%\ferrocast-setup.log)' }
foreach ($f in @($main, 'uninstall.exe', '.ferrocast-setup.json')) {
  if (-not (Test-Path (Join-Path $dir $f))) { Fail "install left no $f" }
}
$entry = Get-ItemProperty $uninstKey
if ($entry.DisplayVersion -ne $version) { Fail "installed-apps entry says $($entry.DisplayVersion)" }
if ($entry.InstallLocation.Trim('"') -ne $dir) { Fail "installed-apps entry points at $($entry.InstallLocation)" }
if (-not (Test-Path $lnk)) { Fail 'no Start menu shortcut' }
Write-Host 'Install: OK'

# An update as the app's updater starts it (no window), over the running files' folder.
Set-Content (Join-Path $dir 'left-over.txt') 'from the old version'
if ((Run $nsis.FullName @('/S', '/UPDATE')) -ne 0) { Fail 'silent update failed' }
if (Test-Path (Join-Path $dir 'left-over.txt')) { Fail 'the update left an old file behind' }
if ((Test-Path "$dir.previous") -or (Test-Path "$dir.incoming")) { Fail 'the update left a folder beside the app' }
Write-Host 'Update: OK'

# Uninstall the way Windows' "Uninstall" button does (quietly here). It hands over to a copy
# in the temp folder, so wait for the folder to go.
$quiet = (Get-ItemProperty $uninstKey).QuietUninstallString
Write-Host "Uninstalling: $quiet"
Start-Process -FilePath (Join-Path $dir 'uninstall.exe') -ArgumentList @('--uninstall', '/S') -Wait
$until = (Get-Date).AddSeconds(60)
while ((Test-Path $dir) -and (Get-Date) -lt $until) { Start-Sleep -Milliseconds 500 }
if (Test-Path $dir) { Get-Content "$env:TEMP\ferrocast-setup.log" -ErrorAction SilentlyContinue; Fail 'uninstall left the app folder' }
Start-Sleep -Seconds 2
if (Test-Path $uninstKey) { Fail 'uninstall left the installed-apps entry' }
if (Test-Path $lnk) { Fail 'uninstall left the Start menu shortcut' }
Write-Host 'Uninstall: OK'
Get-Content "$env:TEMP\ferrocast-setup.log" -ErrorAction SilentlyContinue | Select-Object -Last 20
