<#
.SYNOPSIS
Control CLI for the pooprusteek harness sandbox (Docker Desktop).

.DESCRIPTION
One entry point for every sandbox operation, so a run never depends on
remembering the right `docker compose` incantation. Traces and reports always
land in `sandbox/out/`, which is bind-mounted, so results survive the
container.

.EXAMPLE
./sandbox.ps1 build -BuildProfile dev
./sandbox.ps1 exec "List the files here and name the package version"
./sandbox.ps1 suite live -Repeat 5
./sandbox.ps1 mock malformed-then-recovers
./sandbox.ps1 mine -Sessions
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [ValidateSet('build', 'doctor', 'shell', 'exec', 'scenario', 'suite', 'mine', 'mock', 'stop', 'report', 'reset')]
    [string]$Command,

    [Parameter(Position = 1, ValueFromRemainingArguments = $true)]
    [string[]]$Rest = @(),

    # build: cargo profile. `dev` rebuilds far faster; `release` is realistic.
    # Named BuildProfile, not Profile: `$PROFILE` is an automatic PowerShell
    # variable and a parameter of that name shadows it.
    [ValidateSet('dev', 'release')]
    [string]$BuildProfile = 'release',

    # scenario/suite: how many times each scenario runs. One sample of a
    # nondeterministic model is not evidence.
    [int]$Repeat = 3,

    # mine: also scan the saved session corpus in the data volume.
    [switch]$Sessions
)

$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$composeFile = Join-Path $here 'docker-compose.yml'
$outDir = Join-Path $here 'out'
$envFile = Join-Path $here '.env'

function Invoke-Compose {
    param([string[]]$ComposeArgs)
    $full = @('compose', '-f', $composeFile)
    if (Test-Path $envFile) { $full += @('--env-file', $envFile) }
    $full += $ComposeArgs
    Write-Verbose "docker $($full -join ' ')"
    & docker @full
    return $LASTEXITCODE
}

# One-shot harness invocation. `--rm` because state that must survive lives in
# the data volume or in out/, never in a container.
function Invoke-Harness {
    param([string[]]$HarnessArgs)
    if (-not (Test-Path $outDir)) { New-Item -ItemType Directory -Path $outDir | Out-Null }
    return Invoke-Compose (@('run', '--rm', 'sandbox') + $HarnessArgs)
}

function Get-Stamp { (Get-Date -Format 'yyyy-MM-ddTHH-mm-ss-fffZ') }

# Accept either a bare scenario name or a path, and translate it to the
# container's view of sandbox/scenarios.
function Resolve-ScenarioPath {
    param([string]$Name)
    if ($Name -match '^/opt/sandbox/') { return $Name }
    $candidates = Get-ChildItem -Path (Join-Path $here 'scenarios') -Recurse -Filter '*.toml' |
        Where-Object { $_.BaseName -eq $Name -or $_.Name -eq $Name }
    if ($candidates.Count -eq 1) {
        $relative = $candidates[0].FullName.Substring($here.Length).Replace('\', '/').TrimStart('/')
        return "/opt/sandbox/$($relative -replace '^scenarios/', 'scenarios/')"
    }
    if ($candidates.Count -gt 1) {
        throw "Ambiguous scenario '$Name': $($candidates.Name -join ', ')"
    }
    # Fall back to treating it as a host-relative path under sandbox/.
    $relative = $Name.Replace('\', '/').TrimStart('./')
    return "/opt/sandbox/$relative"
}

switch ($Command) {
    'build' {
        $env:BUILD_PROFILE = $BuildProfile
        Write-Host "Building sandbox image (profile: $BuildProfile)..." -ForegroundColor Cyan
        $code = Invoke-Compose @('build', 'sandbox')
        if ($code -ne 0) { exit $code }
        Write-Host 'Image ready: pooprusteek-sandbox:latest' -ForegroundColor Green
    }

    'doctor' {
        Write-Host '── sandbox doctor ──' -ForegroundColor Cyan
        try { & docker version --format '{{.Server.Version}}' | ForEach-Object { Write-Host "docker engine : $_" } }
        catch { Write-Host 'docker engine : NOT REACHABLE — start Docker Desktop' -ForegroundColor Red; exit 1 }

        $image = & docker images -q pooprusteek-sandbox:latest
        if ($image) { Write-Host "image         : present ($image)" }
        else { Write-Host 'image         : missing — run: ./sandbox.ps1 build' -ForegroundColor Yellow }

        if (Test-Path $envFile) {
            $hasToken = (Get-Content $envFile -Raw) -match 'POOPRUSTEEK_TOKEN=\S'
            if ($hasToken) { Write-Host 'token         : set in sandbox/.env' }
            else { Write-Host 'token         : .env exists but POOPRUSTEEK_TOKEN is empty' -ForegroundColor Yellow }
        }
        else {
            Write-Host 'token         : no sandbox/.env — copy .env.example (mock runs work without it)' -ForegroundColor Yellow
        }

        $scenarios = (Get-ChildItem -Path (Join-Path $here 'scenarios') -Recurse -Filter '*.toml').Count
        Write-Host "scenarios     : $scenarios"
        if (Test-Path $outDir) {
            $reports = (Get-ChildItem -Path $outDir -Recurse -Filter 'report.json' -ErrorAction SilentlyContinue).Count
            Write-Host "reports in out: $reports"
        }
    }

    'shell' {
        # Interactive poke-around inside the same environment scenarios run in.
        exit (Invoke-Compose @('run', '--rm', '-it', 'sandbox', 'bash'))
    }

    'exec' {
        if ($Rest.Count -lt 1) { throw 'exec needs a prompt' }
        $prompt = $Rest[0]
        $extra = if ($Rest.Count -gt 1) { $Rest[1..($Rest.Count - 1)] } else { @() }
        $trace = "/out/exec-$(Get-Stamp).jsonl"
        exit (Invoke-Harness (@('exec', $prompt, '--trace', $trace) + $extra))
    }

    'scenario' {
        if ($Rest.Count -lt 1) { throw 'scenario needs a name or path' }
        $path = Resolve-ScenarioPath $Rest[0]
        $extra = if ($Rest.Count -gt 1) { $Rest[1..($Rest.Count - 1)] } else { @() }
        exit (Invoke-Harness (@('scenario', $path, '--repeat', "$Repeat", '--out', '/out') + $extra))
    }

    'suite' {
        $which = if ($Rest.Count -ge 1) { $Rest[0] } else { 'live' }
        $dir = switch ($which) {
            'live' { '/opt/sandbox/scenarios/live' }
            'mock' { '/opt/sandbox/scenarios/mock' }
            'dev' { '/opt/sandbox/scenarios/dev' }
            'all' { '/opt/sandbox/scenarios' }
            default { throw "suite takes live | mock | dev | all, got '$which'" }
        }
        if ($which -ne 'live') {
            Write-Host 'Note: mock scenarios need the mock service — ./sandbox.ps1 mock <script>' -ForegroundColor Yellow
        }
        $extra = if ($Rest.Count -gt 1) { $Rest[1..($Rest.Count - 1)] } else { @() }
        exit (Invoke-Harness (@('suite', $dir, '--repeat', "$Repeat", '--out', '/out') + $extra))
    }

    'mine' {
        # Not `$args`: that is an automatic PowerShell variable too.
        $mineArgs = @('mine', '/out')
        if ($Sessions) { $mineArgs += '--sessions' }
        exit (Invoke-Harness ($mineArgs + $Rest))
    }

    'mock' {
        if ($Rest.Count -lt 1) {
            Write-Host 'Available scripts:' -ForegroundColor Cyan
            Get-ChildItem -Path (Join-Path $here 'mock-scripts') -Filter '*.toml' |
                ForEach-Object { Write-Host "  $($_.BaseName)" }
            exit 0
        }
        $script = $Rest[0]
        if (-not $script.EndsWith('.toml')) { $script = "$script.toml" }
        $env:MOCK_SCRIPT = $script
        Write-Host "Starting mock provider with $script..." -ForegroundColor Cyan
        exit (Invoke-Compose @('up', '-d', 'mock'))
    }

    'stop' {
        exit (Invoke-Compose @('down', '--remove-orphans'))
    }

    'report' {
        if (-not (Test-Path $outDir)) { Write-Host 'No runs yet.'; exit 0 }
        $reports = Get-ChildItem -Path $outDir -Recurse -Filter '*.json' |
            Sort-Object LastWriteTime -Descending | Select-Object -First 10
        if (-not $reports) { Write-Host 'No reports in sandbox/out.'; exit 0 }
        foreach ($report in $reports) {
            $data = Get-Content $report.FullName -Raw | ConvertFrom-Json
            if ($null -ne $data.scenarios) {
                $mark = if ($data.passed) { 'PASS' } else { 'FAIL' }
                Write-Host "[$mark] suite  $($data.passed_scenarios)/$($data.total)  $($report.Name)"
            }
            elseif ($null -ne $data.pass_rate) {
                $mark = if ($data.passed) { 'PASS' } else { 'FAIL' }
                $rate = [math]::Round($data.pass_rate * 100)
                Write-Host "[$mark] $($data.name)  $($data.passed_runs)/$($data.repeats) ($rate%)"
            }
        }
        Write-Host "`nFull reports under $outDir"
    }

    'reset' {
        Write-Host 'This removes the data volume (embedding model, sessions, index) and sandbox/out.' -ForegroundColor Yellow
        $answer = Read-Host 'Type "yes" to continue'
        if ($answer -ne 'yes') { Write-Host 'Cancelled.'; exit 0 }
        Invoke-Compose @('down', '-v', '--remove-orphans') | Out-Null
        if (Test-Path $outDir) { Remove-Item -Recurse -Force $outDir }
        Write-Host 'Reset done. Next run re-downloads the embedding model.' -ForegroundColor Green
    }
}
