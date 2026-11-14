<#
  Снимки экрана TUI: кадры из тестового бэкенда -> HTML -> PNG через headless Chrome.
  Каждый запуск даёт одинаковые файлы, так что до/после правки их можно сравнивать.

  Примеры:
    pwsh scripts/tui-capture.ps1 -Out .dev/research/ui/baseline
    pwsh scripts/tui-capture.ps1 -Out .dev/research/ui/after-1 -Only chat-working
    pwsh scripts/tui-capture.ps1 -Out .dev/research/ui/after-1 -SkipBuild   # только PNG из готовых HTML
#>
param(
    [string]$Out = ".dev/research/ui/baseline",
    # Подстрока имени сцены (landing, chat-working, approval, ...); пусто — все.
    [string]$Only = "",
    # Не гонять cargo: взять готовые HTML из -Out.
    [switch]$SkipBuild,
    [int]$Parallel = 4
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
New-Item -ItemType Directory -Force $Out | Out-Null
$outFull = (Resolve-Path $Out).Path

function Import-Vcvars {
    # Без окружения MSVC cargo подхватывает GNU link.exe из Git и падает на kernel32.lib.
    if ($env:LIB) { return }
    # ort (prebuilt ONNX Runtime) линкуется только с тулсетом MSVC 14.50+: в VS 2022 (14.41)
    # будет LNK2001 на __std_find_end_1. Нужный стоит в H:\VS Build Tools; свой путь — $env:VCVARS64.
    $bat = @($env:VCVARS64, "H:\VS Build Tools\VC\Auxiliary\Build\vcvars64.bat",
        "C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat") |
        Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
    if (-not $bat) { throw "vcvars64.bat не найден: задай `$env:VCVARS64 или запусти из Developer PowerShell" }
    cmd /c "`"$bat`" >nul 2>&1 && set" | ForEach-Object {
        if ($_ -match '^([^=]+)=(.*)$') { Set-Item -Path "env:$($Matches[1])" -Value $Matches[2] }
    }
}

if (-not $SkipBuild) {
    Import-Vcvars
    $env:TUI_CAPTURE_OUT = $outFull
    if ($Only) { $env:TUI_CAPTURE_ONLY = $Only } else { Remove-Item env:TUI_CAPTURE_ONLY -ErrorAction SilentlyContinue }
    # При STATUS_STACK_BUFFER_OVERRUN (os error 1455) добавь `-j 1` — это нехватка файла подкачки.
    cargo test --bin pooprusteek capture_screens -- --ignored --nocapture
    if ($LASTEXITCODE -ne 0) { throw "cargo test упал ($LASTEXITCODE)" }
}

$chrome = @(
    "C:\Program Files\Google\Chrome\Application\chrome.exe",
    "C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $chrome) { throw "Нужен Chrome или Edge" }

$manifest = Get-Content (Join-Path $outFull "manifest.tsv") | Where-Object { $_ }
$profileRoot = Join-Path $env:TEMP "pooprusteek-capture-chrome"
$manifest | ForEach-Object -Parallel {
    $name, $w, $h = $_ -split "`t"
    $html = Join-Path $using:outFull "$name.html"
    $png = Join-Path $using:outFull "$name.png"
    $profile = Join-Path $using:profileRoot ([guid]::NewGuid().ToString("N"))
    & $using:chrome --headless=new --disable-gpu --hide-scrollbars --force-device-scale-factor=1 `
        --no-first-run --user-data-dir=$profile --window-size="$w,$h" `
        --screenshot=$png "file:///$($html -replace '\\','/')" 2>$null | Out-Null
    Remove-Item $profile -Recurse -Force -ErrorAction SilentlyContinue
    if (-not (Test-Path $png)) { Write-Warning "нет снимка: $name" }
} -ThrottleLimit $Parallel
Write-Host "Готово: $outFull ($($manifest.Count) снимков)"
