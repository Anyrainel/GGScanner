param(
    [Parameter(Mandatory = $true)]
    [ValidateSet("Stop", "Start", "Restart", "BuildAndRestart")]
    [string]$Action,
    [ValidateSet("Genshin", "StarRail")]
    [string]$Game,
    [switch]$RefreshGenshinWorld
)

$ErrorActionPreference = "Stop"
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$scannerDirectory = Join-Path $repoRoot "target\release"
$dataDirectory = Join-Path $scannerDirectory "data"
$scannerPath = Join-Path $scannerDirectory "GGScanner.exe"
$appConfigPath = Join-Path $scannerDirectory "data\good_app_config.json"
$processName = "GGScanner"
$stopTask = "Codex GGScanner Stop"
$startTask = "Codex GGScanner Start"
$worldMarker = Join-Path $dataDirectory "ggscanner-genshin-world.once"
$worldResult = Join-Path $dataDirectory "ggscanner-genshin-world.result"
if ($RefreshGenshinWorld -and ($Game -ne "Genshin" -or $Action -eq "Stop")) {
    throw "-RefreshGenshinWorld requires -Game Genshin and a start or restart action."
}

# The scheduled tasks start GGScanner elevated. Get-Process.Path can be empty
# for such a process when this helper runs at normal user privileges.
if (-not ('GGScanner.ProcessImage' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;
using Microsoft.Win32.SafeHandles;

namespace GGScanner {
    public static class ProcessImage {
        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern SafeProcessHandle OpenProcess(uint access, bool inherit, int pid);

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        private static extern bool QueryFullProcessImageName(
            SafeProcessHandle process, uint flags, StringBuilder path, ref int length);

        public static string GetPath(int pid) {
            const uint QueryLimitedInformation = 0x1000;
            using (var process = OpenProcess(QueryLimitedInformation, false, pid)) {
                if (process.IsInvalid) {
                    int error = Marshal.GetLastWin32Error();
                    if (error == 87) return null; // PID disappeared after enumeration.
                    throw new Win32Exception(error);
                }
                var path = new StringBuilder(32768);
                int length = path.Capacity;
                if (!QueryFullProcessImageName(process, 0, path, ref length))
                    throw new Win32Exception(Marshal.GetLastWin32Error());
                return path.ToString();
            }
        }
    }
}
'@
}

function Get-ProcessImagePath([int]$ProcessId) {
    return [GGScanner.ProcessImage]::GetPath($ProcessId)
}

function Test-ScannerRunning {
    foreach ($process in Get-Process -Name $processName -ErrorAction SilentlyContinue) {
        if ((Get-ProcessImagePath $process.Id) -ieq $scannerPath) {
            return $true
        }
    }
    return $false
}

function Get-ManagerListenerId {
    # Both game servers bind 127.0.0.1. An IPv4 wildcard can also own that endpoint.
    $listeners = Get-NetTCPConnection -State Listen | Where-Object {
        $_.LocalPort -eq 8765 -and $_.LocalAddress -in '127.0.0.1', '0.0.0.0'
    }
    $owner = $null
    foreach ($listener in $listeners) {
        $path = Get-ProcessImagePath $listener.OwningProcess
        if ($path -and $path -ine $scannerPath) {
            throw "Manager port 8765 is already owned by PID $($listener.OwningProcess) ($path). Expected $scannerPath."
        }
        if ($path) { $owner = $listener.OwningProcess }
    }
    return $owner
}

function Wait-ScannerState([bool]$ShouldRun, [int]$TimeoutSeconds = 30) {
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        if ((Test-ScannerRunning) -eq $ShouldRun) {
            return
        }
        Start-Sleep -Milliseconds 500
    } while ((Get-Date) -lt $deadline)

    $expected = if ($ShouldRun) { "running" } else { "stopped" }
    throw "Timed out waiting for GGScanner to be $expected."
}

function Invoke-ScannerTask([string]$TaskName) {
    $task = Get-ScheduledTask -TaskName $TaskName -ErrorAction Stop
    if ($task.State -eq "Disabled") {
        throw "Scheduled task '$TaskName' is disabled."
    }
    Start-ScheduledTask -TaskName $TaskName
}

function Set-ManagerGame {
    if (-not $Game) {
        return
    }
    if (-not (Test-Path -LiteralPath $appConfigPath)) {
        throw "GGScanner settings not found: $appConfigPath"
    }
    $config = Get-Content -LiteralPath $appConfigPath -Raw | ConvertFrom-Json
    if (-not $config.navigation) {
        throw "GGScanner settings have no navigation object: $appConfigPath"
    }
    $config.navigation.activeGame = if ($Game -eq "StarRail") { "starRail" } else { "genshin" }
    $config.navigation.selectedTab = "manager"
    $json = ConvertTo-Json -InputObject $config -Depth 100
    [IO.File]::WriteAllText($appConfigPath, $json + "`n", [Text.UTF8Encoding]::new($false))
}

function Start-ScannerAgent {
    $null = Get-ManagerListenerId
    New-Item -ItemType Directory -Path $dataDirectory -Force | Out-Null
    if (Test-Path -LiteralPath $worldMarker) { Remove-Item -LiteralPath $worldMarker }
    if ($RefreshGenshinWorld) {
        if (Test-Path -LiteralPath $worldResult) { Remove-Item -LiteralPath $worldResult }
        [IO.File]::WriteAllText($worldMarker, "refresh-world-v1")
    }
    [IO.File]::WriteAllText(
        (Join-Path $dataDirectory "ggscanner-agent-mode.once"),
        "agent-mode-v1`n",
        [Text.UTF8Encoding]::new($false)
    )
    try {
        Invoke-ScannerTask $startTask
        Wait-ScannerState $true
        Wait-ManagerHealth
        if ($RefreshGenshinWorld) {
            if (-not (Test-Path -LiteralPath $worldResult)) { throw "No world entry result from GGScanner." }
            $result = (Get-Content -LiteralPath $worldResult -Raw -Encoding UTF8).Trim()
            if ($result -ne "ok") { throw $result }
            Write-Output "Genshin world entry completed."
        }
    }
    finally {
        if (Test-Path -LiteralPath $worldMarker) { Remove-Item -LiteralPath $worldMarker }
    }
}

function Wait-ManagerHealth([int]$TimeoutSeconds = 300) {
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        $owner = Get-ManagerListenerId
        if ($owner) {
            try {
                $health = Invoke-RestMethod -Uri "http://127.0.0.1:8765/health" -TimeoutSec 5
            }
            catch {
                # Startup can spend time checking or downloading the OCR runtime.
                $health = $null
            }
            $gameMatches = switch ($Game) {
                "Genshin" { $health.game -ne "star-rail" }
                "StarRail" { $health.game -eq "star-rail" }
                default { $true }
            }
            if ($health.status -eq "ok" -and $health.enabled -and
                $gameMatches -and
                (Get-ManagerListenerId) -eq $owner) { return }
        }
        Start-Sleep -Seconds 2
    } while ((Get-Date) -lt $deadline)

    $expected = if ($Game -eq "StarRail") { "Star Rail" } else { "Genshin" }
    throw "$expected Manager did not become healthy within $TimeoutSeconds seconds."
}

switch ($Action) {
    "Stop" {
        if (Test-ScannerRunning) {
            Invoke-ScannerTask $stopTask
            Wait-ScannerState $false
        }
        Write-Output "GGScanner stopped."
    }
    "Start" {
        if (-not (Test-Path -LiteralPath $scannerPath)) {
            throw "GGScanner executable not found: $scannerPath"
        }
        if (-not (Test-ScannerRunning)) {
            Set-ManagerGame
            Start-ScannerAgent
        } else {
            if ($RefreshGenshinWorld) { throw "Use -Action Restart to refresh the world with a fresh capture listener." }
            Wait-ManagerHealth
        }
        Write-Output "GGScanner and its selected game's Manager server are running."
    }
    "Restart" {
        if (Test-ScannerRunning) {
            Invoke-ScannerTask $stopTask
            Wait-ScannerState $false
        }
        Set-ManagerGame
        Start-ScannerAgent
        Write-Output "GGScanner and its selected game's Manager server are running."
    }
    "BuildAndRestart" {
        if (-not (Test-Path -LiteralPath $scannerPath)) {
            throw "GGScanner executable not found: $scannerPath"
        }
        if (Test-ScannerRunning) {
            Invoke-ScannerTask $stopTask
            Wait-ScannerState $false
        }
        Set-ManagerGame

        Push-Location $repoRoot
        try {
            & cargo build --locked --release -p good_tools_app --features capture --bin GGScanner
            if ($LASTEXITCODE -ne 0) {
                throw "cargo build failed with exit code $LASTEXITCODE. GGScanner remains stopped."
            }
        }
        finally {
            Pop-Location
        }

        Start-ScannerAgent
        Write-Output "GGScanner rebuilt and restarted with its selected game's Manager server running."
    }
}
