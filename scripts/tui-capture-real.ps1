<#
  Снимки НАСТОЯЩЕГО приложения в conhost (окно на рабочем столе + CopyFromScreen).
  Нужны для того, что тестовый бэкенд не показывает: шрифт консоли, пропавшие глифы, живой ввод.
  Недетерминировано (время, размер окна) и забирает фокус — для сравнения «до/после» использовать tui-capture.ps1.
  Токен и данные одноразовые: конфиг из scripts/ui-capture, данные во временной папке.

    pwsh scripts/tui-capture-real.ps1 -Out .dev/research/ui/baseline
#>
param(
    [string]$Out = ".dev/research/ui/baseline",
    [string]$Exe = "target/debug/pooprusteek.exe",
    [int]$Cols = 120, [int]$Lines = 40
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$exe = (Resolve-Path (Join-Path $root $Exe)).Path
New-Item -ItemType Directory -Force $Out | Out-Null
$out = (Resolve-Path $Out).Path
$data = Join-Path $env:TEMP "pooprusteek-capture-real-$PID"
New-Item -ItemType Directory -Force $data | Out-Null
$cfg = Join-Path $root "scripts\ui-capture\config.toml"
$script = Join-Path $root "scripts\ui-capture\mock-script.toml"

Add-Type -AssemblyName System.Windows.Forms, System.Drawing
Add-Type @'
using System; using System.Text; using System.Runtime.InteropServices; using System.Collections.Generic;
public class W {
 public delegate bool EnumProc(IntPtr h, IntPtr l);
 [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc p, IntPtr l);
 [DllImport("user32.dll")] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
 [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
 [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
 [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
 [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L,T,R,B; }
 public static List<IntPtr> Consoles() { var l = new List<IntPtr>(); EnumWindows((h,x)=>{ var s=new StringBuilder(256); GetClassName(h,s,256); if (s.ToString()=="ConsoleWindowClass" && IsWindowVisible(h)) l.Add(h); return true;}, IntPtr.Zero); return l; }
}
'@
function Snap($name) {
    $h = [W]::Consoles() | Select-Object -First 1
    $rc = New-Object W+RECT; [void][W]::GetWindowRect($h, [ref]$rc)
    $bmp = New-Object System.Drawing.Bitmap ($rc.R - $rc.L), ($rc.B - $rc.T)
    [System.Drawing.Graphics]::FromImage($bmp).CopyFromScreen($rc.L, $rc.T, 0, 0, $bmp.Size)
    $bmp.Save((Join-Path $out "$name.png")); $bmp.Dispose()
}

$mock = Start-Process $exe -ArgumentList "mock-provider", "--port", "8811", "--script", "`"$script`"" -PassThru -WindowStyle Hidden
$app = $null
try {
    Start-Sleep 2
    $app = Start-Process conhost.exe -PassThru -ArgumentList "cmd.exe /c mode con cols=$Cols lines=$Lines && `"$exe`" --config `"$cfg`" --data-dir `"$data`""
    Start-Sleep 6
    [void][W]::SetForegroundWindow(([W]::Consoles() | Select-Object -First 1)); Start-Sleep 1
    Snap "real-conhost__landing__${Cols}x$Lines"
    [System.Windows.Forms.SendKeys]::SendWait("run the tests{ENTER}"); Start-Sleep 4
    Snap "real-conhost__approval__${Cols}x$Lines"
    [System.Windows.Forms.SendKeys]::SendWait("y"); Start-Sleep 5
    Snap "real-conhost__answer__${Cols}x$Lines"
}
finally {
    # Закрываем только то, что запустили здесь.
    Get-Process pooprusteek -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe } | Stop-Process -Force
    if ($app) { Stop-Process -Id $app.Id -Force -ErrorAction SilentlyContinue }
    if (Test-Path $data) { Remove-Item -LiteralPath $data -Recurse -Force }
}
Write-Host "Готово: $out"
