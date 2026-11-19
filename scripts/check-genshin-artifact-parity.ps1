# Run against an existing Genshin Manager with Continuous Sync enabled and a ready login.
# Output is private game data. Exit 0 = match, 1 = differences, 2 = incomplete or invalid.
[CmdletBinding()]
param(
    [string]$OutputRoot = (Join-Path $PSScriptRoot "../test-results/artifact-parity"),
    [ValidateRange(1, 65535)][int]$Port = 8765,
    [ValidateRange(1, 1440)][int]$TimeoutMinutes = 90,
    [switch]$Account
)

$ErrorActionPreference = "Stop"
# The comparator uses exit 1 for a completed comparison with differences.
$PSNativeCommandUseErrorActionPreference = $false
if ($Account -and -not $PSBoundParameters.ContainsKey("TimeoutMinutes")) { $TimeoutMinutes = 180 }
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$OutputRoot = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputRoot)
$runId = (Get-Date -Format "yyyyMMdd-HHmmss") + "-" + [guid]::NewGuid().ToString("N")
$runDir = Join-Path $OutputRoot $runId
[IO.Directory]::CreateDirectory($runDir) | Out-Null
$metadataPath = Join-Path $runDir "metadata.json"
$reportPath = Join-Path $runDir "report.txt"
$capturePath = Join-Path $runDir "capture.json"
$ocrPath = Join-Path $runDir "ocr.json"
$utf8 = [Text.UTF8Encoding]::new($false)
$metadata = [ordered]@{
    status = "running"
    stage = "checking prerequisites"
    startedAt = [DateTimeOffset]::UtcNow.ToString("o")
    port = $Port
    scope = if ($Account) { "account" } else { "artifacts" }
}

function Save-Metadata {
    [IO.File]::WriteAllText($metadataPath, (ConvertTo-Json $metadata -Depth 8) + "`n", $utf8)
}

function Read-Capture([string]$Name, [switch]$Export) {
    $path = Join-Path $runDir "capture-$Name.json"
    try {
        Invoke-WebRequest -UseBasicParsing -Uri "http://127.0.0.1:$Port/continuous" -TimeoutSec 30 -OutFile $path
    } catch {
        $code = $_.Exception.Response.StatusCode
        if ($code) {
            throw "GET /continuous returned HTTP $([int]$code). Continuous Sync must be enabled and the current login inventory ready."
        }
        throw "Could not reach the existing Genshin Manager on local port $Port."
    }
    $arguments = @((Join-Path $PSScriptRoot "genshin_capture_baseline.py"), $path)
    if ($Export) { $arguments += @("--export", $capturePath) }
    if ($Account) { $arguments += "--account" }
    $summary = & $python @arguments
    if ($LASTEXITCODE -ne 0) { throw "The $Name packet snapshot is invalid." }
    return ($summary | ConvertFrom-Json)
}

Write-Output "Parity run directory: $runDir"
Save-Metadata
$exitCode = 2
try {
    $python = (Get-Command python -CommandType Application | Select-Object -First 1).Source
    $tool = if ($Account) { "compare_account_exports.py" } else { "compare_artifact_exports.py" }
    $comparator = Join-Path $repoRoot "tools/$tool"
    & $python -B $comparator --help | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "The selected comparator and its Python dependencies must be available." }

    $metadata.stage = "reading capture baseline"
    $metadata.before = Read-Capture "before" -Export
    $categories = @($metadata.before.categories.PSObject.Properties.Name)
    $metadata.requestedCategories = $categories
    $metadata.coverage = [ordered]@{}
    $metadata.ocrPhases = [ordered]@{}
    $metadata.ocrCounts = [ordered]@{}
    $scanTargets = @{}
    foreach ($name in $categories) {
        $scanTargets[$name] = $true
        $metadata.coverage[$name] = "requested"
    }
    $missingAchievements = $Account -and $categories -notcontains "achievements"
    if ($missingAchievements) {
        $metadata.coverage["achievements"] = "missing_capture_baseline"
        Write-Output "Achievements have no starting packet baseline. Account coverage is incomplete."
    }
    $metadata.stage = "scanning $($categories -join ', ') with OCR"
    Save-Metadata
    & (Join-Path $PSScriptRoot "scan-ggscanner-agent.ps1") -Game Genshin @scanTargets -Port $Port `
        -TimeoutMinutes $TimeoutMinutes -ResultPath $ocrPath -NoResultJson
    $scanCode = $LASTEXITCODE
    if (Test-Path -LiteralPath $ocrPath) {
        $ocr = Get-Content -LiteralPath $ocrPath -Raw -Encoding UTF8 | ConvertFrom-Json
        $metadata.jobId = $ocr.jobId
    }
    if (-not $metadata.jobId) { throw "No OCR job result for the requested categories." }
    $failed = @()
    foreach ($name in $categories) {
        $outcome = @($ocr.job.results | Where-Object { $_.id -ceq $name })
        $phase = if ($outcome.Count -eq 1) { [string]$outcome[0].status } else { "missing_result" }
        if ($phase -ceq "success" -and $ocr.$name -isnot [array]) { $phase = "missing_data" }
        $metadata.ocrPhases[$name] = $phase
        if ($phase -cne "success") {
            $failed += $name
        } else {
            $metadata.ocrCounts[$name] = $ocr.$name.Count
        }
    }
    if ($failed.Count -gt 0) { throw "Unsuccessful OCR categories: $($failed -join ', '). See ocr.json." }
    if ($scanCode -ne 0) { throw "The OCR job was unsuccessful. See ocr.json." }
    $metadata.ocrCount = $ocr.artifacts.Count
    $metadata.stage = "checking final packet inventory"
    Save-Metadata
    $metadata.after = Read-Capture "after"
    if ($metadata.before.sessionId -cne $metadata.after.sessionId -or $metadata.before.uid -cne $metadata.after.uid) {
        throw "The game login session or verified UID changed during OCR."
    }
    if ($metadata.after.revision -lt $metadata.before.revision) {
        throw "The continuous capture revision regressed within the same login."
    }
    foreach ($name in $categories) {
        $before = $metadata.before.categories.$name
        $after = $metadata.after.categories.$name
        if ($null -eq $after -or $before.sha256 -cne $after.sha256) {
            throw "Packet inventory changed or became unavailable during OCR for $name."
        }
    }
    $metadata.stage = "comparing $($metadata.scope)"
    Save-Metadata
    $report = & $python -B $comparator $ocrPath $capturePath
    $compareCode = $LASTEXITCODE
    if ($compareCode -notin @(0, 1)) { throw "The selected comparator failed (exit $compareCode)." }
    [IO.File]::WriteAllLines($reportPath, [string[]]$report, $utf8)
    $metadata.status = if ($compareCode -eq 0) { "match" } else { "differences" }
    foreach ($name in $categories) { $metadata.coverage[$name] = "compared" }
    $metadata.stage = "finished"
    $exitCode = $compareCode
    if ($missingAchievements -and $compareCode -eq 0) {
        $metadata.status = "incomplete"
        $exitCode = 2
    }
} catch {
    $metadata.status = "failed"
    $metadata.error = $_.Exception.Message
    [IO.File]::WriteAllText($reportPath, $metadata.error + "`n", $utf8)
} finally {
    $metadata.finishedAt = [DateTimeOffset]::UtcNow.ToString("o")
    Save-Metadata
}
Write-Output "Result: $($metadata.status). Report: $reportPath"
exit $exitCode
