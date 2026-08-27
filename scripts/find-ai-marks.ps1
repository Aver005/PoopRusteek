<#
.SYNOPSIS
Ищет в последних N коммитах со-авторство и прочие следы ИИ-агентов.

.DESCRIPTION
Проверяет трейлеры Co-authored-by / Generated-by, адреса ИИ-сервисов, фразы о
генерации, эмодзи-робота и ИИ-имена в подписи автора или коммитера.
Коды возврата: 0 — чисто, 1 — что-то найдено, 2 — ошибка запуска.

.PARAMETER Count
Сколько последних коммитов проверять. По умолчанию 20.

.EXAMPLE
scripts\find-ai-marks.ps1 50
#>
[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [int]$Count = 20
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if ($Count -le 0) {
    Write-Error "N должно быть целым числом больше нуля, получено: $Count"
    exit 2
}

git rev-parse --git-dir *> $null
if ($LASTEXITCODE -ne 0) {
    Write-Error "Не репозиторий git: $PWD"
    exit 2
}

$marks = @(
    @{ Label = 'со-авторство'; Pattern = '^co-authored-by:' }
    @{ Label = 'трейлер генерации'; Pattern = '^(generated-by|assisted-by|created-by|ai-assisted-by|x-generated-by):' }
    @{ Label = 'адрес ИИ-сервиса'; Pattern = '(claude\.(ai|com)|anthropic\.com|openai\.com|chat\.openai|cursor\.(com|sh)|codeium\.com|aider\.chat|copilot@|devin-ai-integration|\[bot\]@)' }
    @{ Label = 'фраза о генерации'; Pattern = '(generated (with|by)|co-?authored with|written (with|by)|assisted by)\W*(claude|copilot|chatgpt|gpt-[0-9]|cursor|codex|gemini|devin|aider|нейросет|ии\b|ai\b)' }
    @{ Label = 'эмодзи-робот'; Pattern = '🤖' }
    @{ Label = 'ИИ в подписи'; Pattern = '^(author|committer): .*(claude|copilot|chatgpt|codex|gemini|devin|aider|\[bot\])' }
)

$marker = '@@@pooprusteek-commit@@@'
$format = "$marker%n%h%n%s%nauthor: %an <%ae>%ncommitter: %cn <%ce>%n%b"

$previousEncoding = [Console]::OutputEncoding
try {
    [Console]::OutputEncoding = [Text.Encoding]::UTF8
    $log = @(git log -n $Count --format=$format)
    if ($LASTEXITCODE -ne 0) { exit 2 }
} finally {
    [Console]::OutputEncoding = $previousEncoding
}

$scanned = 0
$found = 0
$short = ''
$subject = ''
$lines = [System.Collections.Generic.List[string]]::new()

# Печатает один коммит, если в его строках нашлась хоть одна метка.
function Show-Commit {
    if (-not $script:short) { return }
    $script:scanned++
    $hits = foreach ($line in $script:lines) {
        $labels = @($marks | Where-Object { $line -match $_.Pattern } | ForEach-Object { $_.Label })
        if ($labels.Count) { "    [$($labels -join ', ')] $line" }
    }
    if ($hits) {
        $script:found++
        Write-Host "$($script:short) $($script:subject)"
        $hits | ForEach-Object { Write-Host $_ }
        Write-Host ''
    }
}

$field = -1
foreach ($line in $log) {
    if ($line -ceq $marker) {
        Show-Commit
        $short = ''
        $subject = ''
        $lines.Clear()
        $field = 0
        continue
    }
    switch ($field) {
        0 { $short = $line; $field = 1 }
        1 { $subject = $line; $lines.Add($line); $field = 2 }
        default { if ($line) { $lines.Add($line) } }
    }
}
Show-Commit

if ($found -eq 0) {
    Write-Host "Проверено коммитов: $scanned — меток ИИ не найдено."
    exit 0
}

Write-Host "Проверено коммитов: $scanned, с метками ИИ: $found."
exit 1
