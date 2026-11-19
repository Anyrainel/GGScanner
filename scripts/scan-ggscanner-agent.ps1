param(
    [Parameter(Mandatory = $true)]
    [ValidateSet("Genshin", "StarRail")]
    [string]$Game,
    [switch]$Characters,
    [switch]$Weapons,
    [switch]$Artifacts,
    [switch]$Achievements,
    [int]$TimeoutMinutes = 90,
    [ValidateRange(1, 65535)][int]$Port = 8765,
    [string]$ResultPath,
    [switch]$NoResultJson
)

$ErrorActionPreference = "Stop"
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$scannerDirectory = Join-Path $repoRoot "target\release"
$dataDirectory = Join-Path $scannerDirectory "data"
$baseUrl = "http://127.0.0.1:$Port"

if (-not ($Characters -or $Weapons -or $Artifacts -or $Achievements)) {
    $Characters = $true
    $Weapons = $true
    $Artifacts = $true
    if ($Game -eq "Genshin") {
        $Achievements = $true
    }
}

function Invoke-Api([string]$Method, [string]$Path, $Body = $null) {
    $parameters = @{
        Method = $Method
        Uri = "$baseUrl$Path"
        TimeoutSec = 15
        ErrorAction = "Stop"
    }
    if ($null -ne $Body) {
        $parameters.ContentType = "application/json"
        $parameters.Body = ConvertTo-Json -InputObject $Body -Depth 100 -Compress
    }
    $response = Invoke-RestMethod @parameters
    return ,$response
}

function Get-JobId($Response) {
    if ($Response.jobId) {
        return [string]$Response.jobId
    }
    throw "The manager did not return a jobId."
}

$healthDeadline = (Get-Date).AddMinutes(5)
do {
    try {
        $health = Invoke-Api GET "/health"
        break
    }
    catch {
        if ((Get-Date) -ge $healthDeadline) {
            throw "Manager health endpoint did not become available: $($_.Exception.Message)"
        }
        Start-Sleep -Seconds 2
    }
} while ((Get-Date) -lt $healthDeadline)
if ($Game -eq "Genshin") {
    if ($health.game -eq "star-rail") {
        throw "Port $Port is serving Star Rail. Start the Genshin manager before scanning."
    }
    if (-not $health.enabled) {
        throw "Genshin manager is disabled."
    }
    if (-not $health.gameAlive) {
        throw "Genshin Impact was not detected. Open the game and retry."
    }
} else {
    if ($health.game -ne "star-rail") {
        throw "Port $Port is serving '$($health.game)', not Star Rail. Restart with -Game StarRail."
    }
    if (-not $health.enabled) {
        throw "Star Rail manager is disabled."
    }
}
if ($health.busy) {
    throw "The selected manager already has a running task."
}

if ($Game -eq "Genshin") {
    $body = @{
        characters = [bool]$Characters
        weapons = [bool]$Weapons
        artifacts = [bool]$Artifacts
        achievements = [bool]$Achievements
    }
} else {
    $body = @{
        characters = [bool]$Characters
        lightCones = [bool]$Weapons
        relics = [bool]$Artifacts
    }
}

$accepted = Invoke-Api POST "/scan" $body
$jobId = Get-JobId $accepted
$deadline = (Get-Date).AddMinutes($TimeoutMinutes)
$lastProgress = ""
do {
    Start-Sleep -Seconds 2
    $status = Invoke-Api GET "/status"
    if ($status.jobId -ne $jobId) {
        throw "Manager job changed while scanning (expected $jobId, found $($status.jobId))."
    }

    if ($Game -eq "Genshin") {
        $progress = $status.scanProgress | ConvertTo-Json -Depth 8 -Compress
        $phase = [string]$status.state
    } else {
        $progress = $status.progress | ConvertTo-Json -Depth 8 -Compress
        $phase = [string]$status.phase
    }
    if ($progress -ne $lastProgress) {
        Write-Output "[$phase] $progress"
        $lastProgress = $progress
    }
    if ($phase -in @("failed", "completed", "idle")) {
        break
    }
} while ((Get-Date) -lt $deadline)

if ((Get-Date) -ge $deadline) {
    throw "Scan $jobId exceeded the $TimeoutMinutes minute timeout."
}

$result = Invoke-Api GET "/result?jobId=$([uri]::EscapeDataString($jobId))"
$categoryFailed = $false
if ($Game -eq "Genshin" -and $phase -eq "completed") {
    $requested = @()
    if ($Characters) { $requested += "characters" }
    if ($Weapons) { $requested += "weapons" }
    if ($Artifacts) { $requested += "artifacts" }
    if ($Achievements) { $requested += "achievements" }
    $data = [ordered]@{ jobId = $jobId; job = $result }
    foreach ($category in $requested) {
        $outcome = @($result.results | Where-Object { $_.id -eq $category })
        if ($outcome.Count -ne 1 -or $outcome[0].status -ne "success") {
            $categoryFailed = $true
            continue
        }
        $items = Invoke-Api GET "/$category`?jobId=$([uri]::EscapeDataString($jobId))"
        if ($items -isnot [array]) {
            throw "The manager returned invalid $category data for the scan job."
        }
        # Windows PowerShell adds extended properties to REST arrays. A plain
        # copy serializes as a JSON array on both 5.1 and PowerShell 7.
        $data[$category] = $items.Clone()
    }
    $result = $data
}

if (-not $ResultPath) {
    $gameName = if ($Game -eq "Genshin") { "genshin" } else { "star-rail" }
    $timestamp = Get-Date -Format "yyyyMMdd-HHmmss"
    $ResultPath = Join-Path $dataDirectory "agent-scan-$gameName-$timestamp.json"
}
$ResultPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($ResultPath)
[IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($ResultPath)) | Out-Null
$resultJson = ConvertTo-Json -InputObject $result -Depth 100
[IO.File]::WriteAllText($resultPath, $resultJson + "`n", [Text.UTF8Encoding]::new($false))
Write-Output "Result saved: $resultPath"
if (-not $NoResultJson) {
    Write-Output (ConvertTo-Json -InputObject $result -Depth 20 -Compress)
}

if ($phase -eq "failed" -or $categoryFailed) {
    exit 1
}
exit 0
