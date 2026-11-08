# Packs Ferrocast as an MSIX for the Microsoft Store (packaging/msix): the built program, the
# pinned FFmpeg build the app would otherwise download (a Store app may not fetch programs to
# run) with its licence, the icons, and the manifest. The Store signs what it's given with
# Microsoft's certificate, so Smart App Control and SmartScreen let it run from the first
# install and Windows shows our own publisher name, at no cost.
#
#   packaging/windows-msix.ps1 -Exe target/release/ferrocast.exe [-Out msix] [-Test]
#
# The package is left unsigned in -Out: that's the file to upload in Partner Center.
# -Test also signs a copy with a throwaway certificate, installs it on this machine, starts it
# the way the Start menu does, checks it runs from its package with FFmpeg beside it, and
# removes it. It installs into Windows, so run -Test on CI or a spare PC, not your own.
#
# The identity comes from Partner Center (the reserved app's Product identity page) through
# MSIX_IDENTITY_NAME, MSIX_PUBLISHER and MSIX_PUBLISHER_DISPLAY_NAME. Without them a test
# identity is used: fine for -Test, refused by the Store.
param(
  [Parameter(Mandatory = $true)][string]$Exe,
  [string]$Out = 'msix',
  [switch]$Test
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot

function Fail($msg) {
  Write-Host "::error::$msg"
  exit 1
}
function Tool($name) {
  # The newest Windows SDK's x64 tools (GitHub's Windows runners have the SDK).
  $found = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\10.*\x64\$name" -ErrorAction SilentlyContinue |
    Sort-Object { [version]$_.Directory.Parent.Name } | Select-Object -Last 1
  if (-not $found) { Fail "$name not found: install the Windows SDK" }
  return $found.FullName
}
function Sha256($path) { (Get-FileHash -Algorithm SHA256 $path).Hash.ToLowerInvariant() }
# GitHub's release downloads sometimes answer 502/504 for a moment: try a few times before failing.
function Download($uri, $dest) {
  for ($try = 1; ; $try++) {
    try { Invoke-WebRequest -Uri $uri -OutFile $dest -UseBasicParsing; return }
    catch {
      if ($try -ge 4) { Fail "couldn't download $uri ($($_.Exception.Message))" }
      Write-Host "Download of $uri failed ($($_.Exception.Message)); trying again in $($try * 10) s"
      Start-Sleep -Seconds ($try * 10)
    }
  }
}

$Exe = (Resolve-Path $Exe).Path
$main = Split-Path -Leaf $Exe

# The version: Cargo.toml's [workspace.package] one. MSIX wants four numbers and the Store
# wants the last one 0, so 3.0.0 and 3.0.0-beta.1 both become 3.0.0.0. Only stable versions
# go to the Store (a later 3.0.0 couldn't replace a beta packed as 3.0.0.0).
$cargo = Get-Content -Raw (Join-Path $repo 'Cargo.toml')
$ws = [regex]::Match($cargo, '(?ms)^\[workspace\.package\](.*?)(^\[|\z)').Groups[1].Value
$semver = [regex]::Match($ws, '(?m)^version\s*=\s*"([^"]+)"').Groups[1].Value
$v = [regex]::Match($semver, '^(\d+)\.(\d+)\.(\d+)')
if (-not $v.Success) { Fail "can't read the version from Cargo.toml" }
$msixVersion = "$($v.Groups[1].Value).$($v.Groups[2].Value).$($v.Groups[3].Value).0"
$prerelease = $semver.Contains('-')

$identity = if ($env:MSIX_IDENTITY_NAME) { $env:MSIX_IDENTITY_NAME } else { 'ClearCraftLabs.Ferrocast.Test' }
$publisher = if ($env:MSIX_PUBLISHER) { $env:MSIX_PUBLISHER } else { 'CN=FerrocastTest' }
$publisherName = if ($env:MSIX_PUBLISHER_DISPLAY_NAME) { $env:MSIX_PUBLISHER_DISPLAY_NAME } else { 'ClearCraft Labs' }
$forStore = [bool]($env:MSIX_IDENTITY_NAME -and $env:MSIX_PUBLISHER)
Write-Host "Packing Ferrocast $semver as $msixVersion ($identity, $publisher, shown as '$publisherName')"
if (-not $forStore) { Write-Host '::notice::No Partner Center identity set: this package is for testing, the Store would refuse it.' }
if ($prerelease) { Write-Host "::notice::$semver is a prerelease: test it, but upload only stable versions to the Store." }

$work = Join-Path ($(if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [IO.Path]::GetTempPath() })) 'fc-msix'
$pkg = Join-Path $work 'package'
Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $pkg, (Join-Path $pkg 'Assets') | Out-Null

# 1. The program.
Copy-Item $Exe (Join-Path $pkg $main)

# 2. FFmpeg: the same pinned build crates/engine/src/ffmpeg_get.rs downloads (read from there,
# so the two never differ), checked against its SHA-256. Beside the program, where the app
# looks first (golive::find_ffmpeg). Its licence and build notes (with the source link) too.
$getter = Get-Content -Raw (Join-Path $repo 'crates/engine/src/ffmpeg_get.rs')
$release = [regex]::Match($getter, 'pub const RELEASE: &str = "([^"]+)"').Groups[1].Value
$sha = [regex]::Match($getter, '"ffmpeg-win32-x64\.gz",\s*"([0-9a-f]{64})"').Groups[1].Value
if (-not $release -or -not $sha) { Fail "can't read the pinned FFmpeg build from ffmpeg_get.rs" }
$base = "https://github.com/eugeneware/ffmpeg-static/releases/download/$release"
$gz = Join-Path $work 'ffmpeg.gz'
Download "$base/ffmpeg-win32-x64.gz" $gz
if ((Sha256 $gz) -ne $sha) { Fail "FFmpeg's download doesn't match its pinned SHA-256" }
$in = [IO.File]::OpenRead($gz)
try {
  $unzip = New-Object IO.Compression.GZipStream($in, [IO.Compression.CompressionMode]::Decompress)
  $outFile = [IO.File]::Create((Join-Path $pkg 'ffmpeg.exe'))
  try { $unzip.CopyTo($outFile) } finally { $outFile.Dispose(); $unzip.Dispose() }
} finally { $in.Dispose() }
# The licence and build notes of that release (pinned too: they ship with the package).
$notes = @{
  'win32-x64.LICENSE' = @('ffmpeg-LICENSE.txt', '8ceb4b9ee5adedde47b31e975c1d90c73ad27b6b165a1dcd80c7c545eb65b903')
  'win32-x64.README'  = @('ffmpeg-README.txt', 'a636a7183c58006351acbaf35303c0ed85c6e1320fd4e80de453ba6157de6311')
}
foreach ($name in $notes.Keys) {
  $dest = Join-Path $pkg $notes[$name][0]
  Download "$base/$name" $dest
  if ((Sha256 $dest) -ne $notes[$name][1]) { Fail "FFmpeg's $name doesn't match its pinned SHA-256" }
}
Write-Host ("FFmpeg {0}: {1:N1} MB" -f $release, ((Get-Item (Join-Path $pkg 'ffmpeg.exe')).Length / 1MB))

# 3. Icons and the manifest.
Copy-Item (Join-Path $PSScriptRoot 'msix/Assets/*.png') (Join-Path $pkg 'Assets')
$xml = Get-Content -Raw (Join-Path $PSScriptRoot 'msix/AppxManifest.xml')
$esc = { param($s) [Security.SecurityElement]::Escape($s) }
$xml = $xml.Replace('{{IDENTITY_NAME}}', (& $esc $identity))
$xml = $xml.Replace('{{PUBLISHER_DISPLAY_NAME}}', (& $esc $publisherName))
$xml = $xml.Replace('{{PUBLISHER}}', (& $esc $publisher))
$xml = $xml.Replace('{{VERSION}}', $msixVersion)
$xml = $xml.Replace('{{EXE}}', (& $esc $main))
if ($xml -match '\{\{[A-Z_]+\}\}') { Fail "the manifest still has $($Matches[0])" }
[IO.File]::WriteAllText((Join-Path $pkg 'AppxManifest.xml'), $xml, (New-Object Text.UTF8Encoding($false)))

# 4. resources.pri, so Windows picks the icon size for each place (taskbar, Start, Settings).
$makepri = Tool 'makepri.exe'
$priconfig = Join-Path $work 'priconfig.xml'
& $makepri createconfig /cf $priconfig /dq en-US /o | Out-Null
if ($LASTEXITCODE -ne 0) { Fail 'makepri createconfig failed' }
& $makepri new /pr $pkg /cf $priconfig /mn (Join-Path $pkg 'AppxManifest.xml') /of (Join-Path $pkg 'resources.pri') /o | Out-Null
if ($LASTEXITCODE -ne 0) { Fail 'makepri new failed' }

# 5. The package (makeappx checks the manifest against the schema as it packs).
New-Item -ItemType Directory -Force $Out | Out-Null
$msix = Join-Path (Resolve-Path $Out).Path "Ferrocast_${semver}_x64.msix"
$makeappx = Tool 'makeappx.exe'
& $makeappx pack /d $pkg /p $msix /o
if ($LASTEXITCODE -ne 0) { Fail 'makeappx pack failed' }
Write-Host ("Package: {0} ({1:N1} MB)" -f $msix, ((Get-Item $msix).Length / 1MB))
if (-not $Test) { exit 0 }

# ---- -Test: install a signed copy, start it, check it, remove it. ----
$signtool = Tool 'signtool.exe'
$signed = Join-Path $work 'Ferrocast-test.msix'
Copy-Item $msix $signed
$cert = New-SelfSignedCertificate -Type Custom -Subject $publisher -KeyUsage DigitalSignature `
  -FriendlyName 'Ferrocast MSIX test' -CertStoreLocation 'Cert:\CurrentUser\My' `
  -TextExtension @('2.5.29.37={text}1.3.6.1.5.5.7.3.3', '2.5.29.19={text}')
$pfx = Join-Path $work 'test.pfx'
$cer = Join-Path $work 'test.cer'
$pass = ConvertTo-SecureString -String ([guid]::NewGuid().ToString()) -Force -AsPlainText
Export-PfxCertificate -Cert $cert -FilePath $pfx -Password $pass | Out-Null
Export-Certificate -Cert $cert -FilePath $cer | Out-Null
$plain = [Runtime.InteropServices.Marshal]::PtrToStringAuto([Runtime.InteropServices.Marshal]::SecureStringToBSTR($pass))
& $signtool sign /fd SHA256 /f $pfx /p $plain $signed
if ($LASTEXITCODE -ne 0) { Fail 'signing the test copy failed' }
Import-Certificate -FilePath $cer -CertStoreLocation 'Cert:\LocalMachine\TrustedPeople' | Out-Null

$installed = $null
try {
  Add-AppxPackage -Path $signed
  $installed = Get-AppxPackage -Name $identity
  if (-not $installed) { Fail 'the package installed, but Windows doesn''t list it' }
  Write-Host "Installed: $($installed.PackageFullName) in $($installed.InstallLocation)"
  foreach ($f in @($main, 'ffmpeg.exe', 'ffmpeg-LICENSE.txt', 'resources.pri')) {
    if (-not (Test-Path (Join-Path $installed.InstallLocation $f))) { Fail "the installed package has no $f" }
  }

  # Started as the Start menu does, so it runs with the package's identity.
  Start-Process explorer.exe -ArgumentList "shell:AppsFolder\$($installed.PackageFamilyName)!Ferrocast"
  $proc = $null
  $until = (Get-Date).AddSeconds(60)
  while (-not $proc -and (Get-Date) -lt $until) {
    Start-Sleep -Seconds 1
    $proc = Get-Process -Name ([IO.Path]::GetFileNameWithoutExtension($main)) -ErrorAction SilentlyContinue |
      Where-Object { $_.Path -and $_.Path.StartsWith($installed.InstallLocation, [StringComparison]::OrdinalIgnoreCase) } |
      Select-Object -First 1
  }
  if (-not $proc) { Fail 'Ferrocast didn''t start from its package within 60 s' }
  Write-Host "Running from the package: $($proc.Path) (pid $($proc.Id))"

  # Its log (AppData writes from a package land in the package's own folder) must say it knows
  # it's the Store copy, and it must still be running after it settled.
  $logs = @(
    (Join-Path $env:LOCALAPPDATA "Packages\$($installed.PackageFamilyName)\LocalCache\Roaming\Ferrocast\logs\ferrocast.log"),
    (Join-Path $env:APPDATA 'Ferrocast\logs\ferrocast.log')
  )
  $said = $false
  $until = (Get-Date).AddSeconds(45)
  while (-not $said -and (Get-Date) -lt $until) {
    Start-Sleep -Seconds 1
    foreach ($l in $logs) {
      if ((Test-Path $l) -and (Select-String -Path $l -SimpleMatch 'running from the Microsoft Store package' -Quiet)) { $said = $true; $log = $l }
    }
  }
  if (-not $said) {
    foreach ($l in $logs) { if (Test-Path $l) { Write-Host "--- $l"; Get-Content $l | Select-Object -Last 40 } }
    Fail 'the app didn''t log that it runs from the Store package'
  }
  Start-Sleep -Seconds 10
  if ($proc.HasExited) {
    Get-Content $log | Select-Object -Last 60
    Fail "Ferrocast stopped by itself (exit code $($proc.ExitCode))"
  }
  # Launch at login is off by default, so the first start already turned the package's startup
  # task off through WinRT (src/store.rs): that must have worked.
  if (-not (Select-String -Path $log -SimpleMatch "launch at login set through the package's startup task" -Quiet)) {
    Get-Content $log | Select-Object -Last 30
    Fail 'the startup task (launch at login) could not be changed from the package'
  }
  Write-Host "Log: $log"
  Get-Content $log | Select-Object -Last 15
  Write-Host 'Store package test: OK'
} finally {
  Get-Process -Name ([IO.Path]::GetFileNameWithoutExtension($main)) -ErrorAction SilentlyContinue |
    Where-Object { $installed -and $_.Path -and $_.Path.StartsWith($installed.InstallLocation, [StringComparison]::OrdinalIgnoreCase) } |
    Stop-Process -Force -ErrorAction SilentlyContinue
  Start-Sleep -Seconds 2
  if ($installed) { Remove-AppxPackage -Package $installed.PackageFullName -ErrorAction SilentlyContinue }
  Get-ChildItem 'Cert:\LocalMachine\TrustedPeople', 'Cert:\CurrentUser\My' |
    Where-Object { $_.Thumbprint -eq $cert.Thumbprint } | Remove-Item -ErrorAction SilentlyContinue
}
