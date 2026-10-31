# Puts OBS's engine (obs.dll, D3D11, the plugins the Studio uses, their libraries and OBS's
# Chromium for web-page sources) into Ferrocast's Windows installer, so the Studio works without
# OBS installed. The Windows twin of deploy/obs-runtime-macos.sh.
#
# Takes them unchanged from OBS's portable zip (OBS_VERSION below, the libobs our ffi.rs is
# written against), checked by sha256. Leaves out OBS's own window (obs64.exe), scripting, debug
# symbols (.pdb) and the plugins that only work inside OBS's Qt window; Qt's three core
# libraries stay because obs-browser links them. obs-vst stays out as on macOS.
#
#   deploy/obs-runtime-windows.ps1 [-Out apps/desktop/obs-runtime]
#
# Writes <Out>/obs (installed as <install dir>\obs), <Out>/root (installed beside Ferrocast.exe:
# OBS's GPU encoder checks, which plugins look for beside the running program, and the C++
# runtime they need) and <Out>/tauri.obs.json for `cargo tauri build --config`.
param([string]$Out = (Join-Path $PSScriptRoot '..\apps\desktop\obs-runtime'))
$ErrorActionPreference = 'Stop'

$ObsVersion = '32.2.2'
$Sha = '4d6e40e3ab155f56b30de517380566a206d74b63cdf5ad49aa596924768f97e1'

$Bin = @('obs.dll', 'libobs-d3d11.dll', 'libobs-winrt.dll', 'w32-pthreads.dll', 'zlib.dll',
  'avcodec-62.dll', 'avdevice-62.dll', 'avfilter-11.dll', 'avformat-62.dll', 'avutil-60.dll',
  'swresample-6.dll', 'swscale-9.dll', 'libx264-164.dll', 'srt.dll', 'librist.dll',
  'datachannel.dll', 'libcurl.dll', 'obs-ffmpeg-mux.exe',
  # obs-browser links these (with no window of Qt's own: it only uses them for OBS's docks).
  'Qt6Core.dll', 'Qt6Gui.dll', 'Qt6Widgets.dll', 'obs-frontend-api.dll')
$Plugins = @('decklink', 'image-source', 'nv-filters', 'obs-browser', 'obs-ffmpeg', 'obs-filters',
  'obs-nvenc', 'obs-outputs', 'obs-qsv11', 'obs-text', 'obs-transitions', 'obs-webrtc', 'obs-x264',
  'rtmp-services', 'text-freetype2', 'vlc-video', 'win-capture', 'win-dshow', 'win-wasapi')
# Chromium (CEF) beside obs-browser.dll: its library, data and helper process.
$Cef = @('libcef.dll', 'chrome_elf.dll', 'libEGL.dll', 'libGLESv2.dll', 'icudtl.dat',
  'v8_context_snapshot.bin', 'resources.pak', 'chrome_100_percent.pak', 'chrome_200_percent.pak',
  'obs-browser-page.exe', 'locales')
$Checks = @('obs-nvenc-test.exe', 'obs-amf-test.exe', 'obs-qsv-test.exe')

$cache = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [IO.Path]::GetTempPath() }
$zip = Join-Path $cache "OBS-Studio-$ObsVersion-Windows-x64.zip"
function Hash($f) { (Get-FileHash $f -Algorithm SHA256).Hash.ToLower() }
if (-not (Test-Path $zip) -or (Hash $zip) -ne $Sha) {
  $url = "https://github.com/obsproject/obs-studio/releases/download/$ObsVersion/OBS-Studio-$ObsVersion-Windows-x64.zip"
  Invoke-WebRequest -Uri $url -OutFile "$zip.part" -UseBasicParsing
  Move-Item -Force "$zip.part" $zip
}
$got = Hash $zip
if ($got -ne $Sha) { throw "OBS zip checksum mismatch: $got" }

$src = Join-Path $cache "obs-$ObsVersion-windows"
if (Test-Path $src) { Remove-Item -Recurse -Force $src }
Expand-Archive $zip -DestinationPath $src

New-Item -ItemType Directory -Force $Out | Out-Null
$Out = (Resolve-Path $Out).Path
$obs = Join-Path $Out 'obs'
$root = Join-Path $Out 'root'
foreach ($d in @($obs, $root)) { if (Test-Path $d) { Remove-Item -Recurse -Force $d } }
$binOut = Join-Path $obs 'bin\64bit'
$plugOut = Join-Path $obs 'obs-plugins\64bit'
New-Item -ItemType Directory -Force $binOut, $plugOut, $root, (Join-Path $obs 'data\obs-plugins') | Out-Null

function Take($from, $to) {
  if (-not (Test-Path $from)) { throw "OBS $ObsVersion has no $from" }
  Copy-Item -Recurse -Force $from $to
}
foreach ($f in $Bin) { Take (Join-Path $src "bin\64bit\$f") $binOut }
Take (Join-Path $src 'data\libobs') (Join-Path $obs 'data\libobs')
foreach ($p in $Plugins) {
  Take (Join-Path $src "obs-plugins\64bit\$p.dll") $plugOut
  $data = Join-Path $src "data\obs-plugins\$p"
  if (Test-Path $data) { Copy-Item -Recurse -Force $data (Join-Path $obs 'data\obs-plugins\') }
}
foreach ($f in $Cef) { Take (Join-Path $src "obs-plugins\64bit\$f") $plugOut }
foreach ($f in $Checks) { Take (Join-Path $src "bin\64bit\$f") $root }

# Remove background's model (Robust Video Matting, MobileNetV3, GPL-3.0), which the Studio
# finds at obs\data\ferrocast (crates/studio/src/ffi/matte.rs), checked by sha256.
$RvmSha = '88d4531297118f595bf2fd60f6f566aec2e559393802d1f436c380f0cbbd2828'
$rvm = Join-Path $cache 'rvm_mobilenetv3_fp32.onnx'
if (-not (Test-Path $rvm) -or (Hash $rvm) -ne $RvmSha) {
  $url = 'https://github.com/PeterL1n/RobustVideoMatting/releases/download/v1.0.0/rvm_mobilenetv3_fp32.onnx'
  Invoke-WebRequest -Uri $url -OutFile "$rvm.part" -UseBasicParsing
  Move-Item -Force "$rvm.part" $rvm
}
$got = Hash $rvm
if ($got -ne $RvmSha) { throw "Remove background's model checksum mismatch: $got" }
$modelOut = Join-Path $obs 'data\ferrocast'
New-Item -ItemType Directory -Force $modelOut | Out-Null
Copy-Item -Force $rvm $modelOut

# Microsoft's C++ runtime (allowed app-local), for OBS's DLLs and the encoder checks: from the
# Visual Studio on this machine (CI's runner has it).
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$vs = & $vswhere -latest -products * -property installationPath
# Version folders (14.44.35112) newest first, then the others beside them (v145).
$crt = Get-ChildItem (Join-Path $vs 'VC\Redist\MSVC') -Directory |
  Sort-Object { if ($_.Name -match '^\d+(\.\d+){1,3}$') { [version]$_.Name } else { [version]'0.0' } } -Descending |
  ForEach-Object { Get-ChildItem (Join-Path $_.FullName 'x64') -Directory -Filter 'Microsoft.VC*.CRT' -ErrorAction SilentlyContinue } |
  Select-Object -First 1
if (-not $crt) { throw "no Visual C++ runtime found under $vs" }
foreach ($d in @($binOut, $root)) { Copy-Item -Force (Join-Path $crt.FullName '*.dll') $d }

# Every DLL the plugins and helpers load must be here or be Windows's own.
$objdump = Get-Command llvm-objdump -ErrorAction SilentlyContinue
if ($objdump) {
  # The engine's DLLs are loaded up front by name (ffi.rs `load_deps`), so anything in obs can
  # use any of them; what's beside Ferrocast.exe finds only its own folder and Windows's.
  function Names($dirs) {
    $n = @{}
    Get-ChildItem -Recurse -File $dirs | ForEach-Object { $n[$_.Name.ToLower()] = $true }
    $n
  }
  $missing = @()
  foreach ($set in @(@{ dir = $obs; names = (Names $obs) }, @{ dir = $root; names = (Names $root) })) {
    Get-ChildItem -Recurse -File $set.dir -Include *.dll, *.exe | ForEach-Object {
      $f = $_.FullName
      & $objdump.Source -p $f | Select-String 'DLL Name: (.+)$' | ForEach-Object {
        $dll = $_.Matches[0].Groups[1].Value.Trim().ToLower()
        $system = (Test-Path (Join-Path $env:SystemRoot "System32\$dll")) -or $dll.StartsWith('api-ms-win-')
        if (-not $set.names[$dll] -and -not $system) { $missing += "$dll (from $f)" }
      }
    }
  }
  if ($missing) { $missing | ForEach-Object { Write-Host $_ }; throw 'some DLLs are missing' }
}

$notice = @"
Ferrocast's Studio runs OBS Studio's engine. The obs folder and obs-*-test.exe are taken
unchanged from OBS Studio $ObsVersion for Windows, by the OBS Project, under the GNU General Public
License version 2 or later. They include FFmpeg, x264, SRT, librist, libcurl, libdatachannel,
Qt and Chromium Embedded Framework, each under its own licence (see OBS Studio's).

Source: https://github.com/obsproject/obs-studio/tree/$ObsVersion
Libraries' sources: https://github.com/obsproject/obs-deps

data\ferrocast\rvm_mobilenetv3_fp32.onnx is Robust Video Matting (MobileNetV3) by Shanchuan Lin
and others, under the GNU General Public License version 3:
https://github.com/PeterL1n/RobustVideoMatting
"@
[IO.File]::WriteAllText((Join-Path $obs 'OBS-NOTICE.txt'), $notice, [Text.UTF8Encoding]::new($false))

# The bundler's copy list: obs → <install dir>\obs, root's files → beside Ferrocast.exe.
$resources = [ordered]@{}
$resources[$obs.Replace('\', '/')] = 'obs'
$resources[$root.Replace('\', '/')] = ''
# Without a byte-order mark: the bundler reads it as JSON.
$json = @{ bundle = @{ resources = $resources } } | ConvertTo-Json -Depth 5
[IO.File]::WriteAllText((Join-Path $Out 'tauri.obs.json'), $json, [Text.UTF8Encoding]::new($false))

$size = (Get-ChildItem -Recurse -File $obs, $root | Measure-Object Length -Sum).Sum / 1MB
Write-Host ("obs-runtime: OBS $ObsVersion engine in $Out ({0:N0} MB)" -f $size)
