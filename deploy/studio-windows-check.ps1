# The Studio in the real Windows app, end to end (CI's Windows runner, no GPU: D3D11 on WARP):
# the app runs on the engine beside it (deploy/obs-runtime-windows.ps1), its picture shows
# in the window, a real click on the picture reaches the page and picks the source under it,
# and an MKV recording goes through OBS's mux helper. Screenshots and logs go to <Out>.
#
#   deploy/studio-windows-check.ps1 -Exe target\debug\ferrocast.exe -Out <folder>
#
# Needs a debug build (FERROCAST_DEBUG_TOKEN_FILE and FERROCAST_DEBUG_ROUTE).
param([Parameter(Mandatory)][string]$Exe, [Parameter(Mandatory)][string]$Out)
$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force $Out | Out-Null
$Out = (Resolve-Path $Out).Path
Add-Type -AssemblyName System.Drawing, System.Windows.Forms
Add-Type @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public static class Win {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc f, IntPtr l);
  [DllImport("user32.dll")] static extern bool EnumChildWindows(IntPtr p, EnumProc f, IntPtr l);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetClassName(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, UIntPtr e);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  public struct RECT { public int Left, Top, Right, Bottom; }
  public static IntPtr Child(uint pid, string cls) {
    IntPtr found = IntPtr.Zero;
    EnumWindows((top, _) => {
      uint p; GetWindowThreadProcessId(top, out p);
      if (p != pid) return true;
      EnumChildWindows(top, (c, __) => {
        var s = new StringBuilder(64); GetClassName(c, s, 64);
        if (s.ToString() == cls) { found = c; return false; }
        return true;
      }, IntPtr.Zero);
      return found == IntPtr.Zero;
    }, IntPtr.Zero);
    return found;
  }
}
'@
[Win]::SetProcessDPIAware() | Out-Null

function Shot($name) {
  $b = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
  $bmp = New-Object System.Drawing.Bitmap $b.Width, $b.Height
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.CopyFromScreen($b.Location, [System.Drawing.Point]::Empty, $b.Size)
  $bmp.Save((Join-Path $Out "$name.png"), [System.Drawing.Imaging.ImageFormat]::Png)
  $g.Dispose(); $bmp.Dispose()
}
function Wait($what, $seconds, [scriptblock]$ok) {
  for ($i = 0; $i -lt $seconds * 2; $i++) { $v = & $ok; if ($v) { return $v }; Start-Sleep -Milliseconds 500 }
  throw "timed out waiting for $what"
}

$home_ = Join-Path $Out 'home'
$tokenFile = Join-Path $Out 'token'
$env:FERROCAST_HOME = $home_
$env:FERROCAST_DEBUG_TOKEN_FILE = $tokenFile
$env:FERROCAST_DEBUG_ROUTE = '#/studio'
$env:RUST_LOG = 'info,ferrocast::studio=debug,tao=warn,wry=warn'
function Launch($log) {
  Remove-Item -Force $tokenFile -ErrorAction SilentlyContinue
  $p = Start-Process -FilePath $Exe -PassThru -RedirectStandardOutput (Join-Path $Out "$log.out.log") -RedirectStandardError (Join-Path $Out "$log.log")
  $tok = Wait 'the app' 90 { if (Test-Path $tokenFile) { (Get-Content $tokenFile -Raw).Trim() } }
  $script:base, $token = $tok -split ' ', 2
  $script:h = @{ Authorization = "Bearer $token" }
  $p
}
function Api($method, $path, $body) {
  $args_ = @{ Method = $method; Uri = "$base/api/v1$path"; Headers = $h; ContentType = 'application/json'; TimeoutSec = 120 }
  if ($null -ne $body) { $args_.Body = ($body | ConvertTo-Json -Depth 8 -Compress) }
  Invoke-RestMethod @args_
}

# A new install opens first-time setup: finished here, then the app opens on the Studio.
$app = Launch 'setup'
Api POST '/setup/complete' $null | Out-Null
Stop-Process -Id $app.Id -Force
Start-Sleep -Seconds 2
$app = Launch 'app'
$failed = $null
try {
  $status = Api POST '/studio/engine/start' @{}
  Write-Host "Studio: $($status | ConvertTo-Json -Compress)"
  if (-not $status.running) { throw "the Studio didn't start: $($status.detail)" }
  $kinds = Api GET '/studio/engine/kinds'
  Write-Host "Encoders: $(($kinds.encoders | ForEach-Object id) -join ', ')"
  Write-Host "Inputs: $(($kinds.inputs | ForEach-Object id) -join ', ')"
  $scene = (Api GET '/studio/engine/scenes').current
  Api POST "/studio/engine/scenes/$([uri]::EscapeDataString($scene))/items" @{ kind = 'color_source'; name = 'Box' } | Out-Null
  # Bigger than the canvas, so the middle of the picture is on it.
  Api PUT '/studio/engine/sources/Box/settings' @{ settings = @{ width = 3840; height = 2160; color = 4278222847 } } | Out-Null

  # The picture: the page puts it over its stage once the Studio runs.
  $view = Wait 'the picture' 60 { $v = [Win]::Child($app.Id, 'FerrocastPreview'); if ($v -ne [IntPtr]::Zero) { $v } }
  Start-Sleep -Seconds 3
  Shot 'studio'
  $r = New-Object Win+RECT
  [Win]::GetWindowRect($view, [ref]$r) | Out-Null
  Write-Host "Picture at $($r.Left),$($r.Top) to $($r.Right),$($r.Bottom)"
  $x = [int](($r.Left + $r.Right) / 2); $y = [int](($r.Top + $r.Bottom) / 2)
  [Win]::SetCursorPos($x - 20, $y - 20) | Out-Null; Start-Sleep -Milliseconds 200
  [Win]::SetCursorPos($x, $y) | Out-Null; Start-Sleep -Milliseconds 200
  [Win]::mouse_event(0x2, 0, 0, 0, [UIntPtr]::Zero); Start-Sleep -Milliseconds 120  # left down
  [Win]::mouse_event(0x4, 0, 0, 0, [UIntPtr]::Zero)                                 # left up
  Start-Sleep -Seconds 2
  Shot 'studio-picked'
  # The debug app on a runner without a GPU can take a few seconds to log it.
  $picked = { (Get-Content (Join-Path $Out 'app.out.log') -Raw) -match 'picked in the preview selected=Some' }
  try { Wait 'the pick' 10 $picked | Out-Null } catch { throw "a click on the picture didn't reach the page" }
  Write-Host 'A click on the picture picked the source under it.'

  # MKV goes through obs-ffmpeg-mux (Ferrocast's stand-in, then OBS's beside the engine).
  $rec = Join-Path $Out 'rec'
  New-Item -ItemType Directory -Force $rec | Out-Null
  $s = Api GET '/studio/engine/settings'
  $s.record_format = 'mkv'; $s.record_folder = $rec; $s.record_remux = $false
  Api PUT '/studio/engine/settings' $s | Out-Null
  Api POST '/studio/engine/record' @{ on = $true } | Out-Null
  Start-Sleep -Seconds 4
  Api POST '/studio/engine/record' @{ on = $false } | Out-Null
  $file = Get-ChildItem $rec -Filter *.mkv | Select-Object -First 1
  if (-not $file -or $file.Length -lt 10KB) { throw 'no MKV recording' }
  Write-Host "Recorded $($file.Name) ($([int]($file.Length / 1KB)) KB)"
} catch {
  $failed = $_
  Shot 'failed'
} finally {
  Stop-Process -Id $app.Id -Force -ErrorAction SilentlyContinue
}
if ($failed) { throw $failed }
