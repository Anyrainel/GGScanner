# During a supervised test session, open the game for the development scanner
# and request a normal exit when that scanner exits. Use -Check for a dry run.
[CmdletBinding()]
param(
    [switch]$Check,
    [ValidateRange(1, 1440)][int]$Minutes = 120
)

$ErrorActionPreference = "Stop"
$repo = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$scannerPath = (Get-Item -LiteralPath (Join-Path $repo "target\release\GGScanner.exe")).FullName
$gameDir = Join-Path ([Environment]::GetFolderPath("ProgramFiles")) "HoYoPlay\games\Genshin Impact game"
$gamePath = (Get-Item -LiteralPath (Join-Path $gameDir "GenshinImpact.exe")).FullName

function Confirm-GamePublisher {
    $signature = Get-AuthenticodeSignature -LiteralPath $gamePath
    if ($signature.Status -ne "Valid" -or
        $signature.SignerCertificate.GetNameInfo("SimpleName", $false) -ne "COGNOSPHERE PTE. LTD.") {
        throw "The installed game does not have the expected Genshin Impact publisher signature: $gamePath"
    }
}

Confirm-GamePublisher
if ($Check) {
    Write-Output "Game: $gamePath"
    Write-Output "Development scanner: $scannerPath"
    return
}
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
if (-not [Security.Principal.WindowsPrincipal]::new($identity).IsInRole(
        [Security.Principal.WindowsBuiltInRole]::Administrator)) {
    $shell = Join-Path ([Environment]::SystemDirectory) "WindowsPowerShell\v1.0\powershell.exe"
    Start-Process -FilePath $shell -Verb RunAs -ArgumentList "-NoProfile -File `"$PSCommandPath`" -Minutes $Minutes"
    return
}

$deadline = (Get-Date).AddMinutes($Minutes)
$trackedScanner = $null
$ownedGame = $null
Write-Output "Genshin test session is active until $deadline. Press Ctrl+C to end it."
try {
    while ((Get-Date) -lt $deadline) {
        if ($trackedScanner -and $trackedScanner.HasExited) {
            if ($ownedGame -and !$ownedGame.HasExited) {
                if (-not $ownedGame.CloseMainWindow() -or !$ownedGame.WaitForExit(15000)) {
                    throw "The game did not exit normally. Finish closing its window before starting another test."
                }
            }
            $trackedScanner.Dispose()
            if ($ownedGame) { $ownedGame.Dispose() }
            $trackedScanner = $null
            $ownedGame = $null
        }
        if (!$trackedScanner) {
            $running = Get-CimInstance Win32_Process -Filter "Name='GGScanner.exe'" |
                Where-Object { $_.ExecutablePath -eq $scannerPath } | Select-Object -First 1
            if ($running) {
                $listener = Get-NetTCPConnection -State Listen -LocalAddress 127.0.0.1 `
                    -OwningProcess $running.ProcessId -ErrorAction SilentlyContinue
                if ($listener) {
                    $trackedScanner = Get-Process -Id $running.ProcessId -ErrorAction SilentlyContinue
                    if ($trackedScanner -and !(Get-Process -Name GenshinImpact -ErrorAction SilentlyContinue)) {
                        Confirm-GamePublisher
                        $ownedGame = Start-Process -FilePath $gamePath -WorkingDirectory $gameDir -PassThru
                        Write-Output "Started Genshin Impact for the development scanner. Enter the game world."
                    }
                }
            }
        }
        Start-Sleep -Seconds 2
    }
} finally {
    if ($trackedScanner) { $trackedScanner.Dispose() }
    if ($ownedGame) { $ownedGame.Dispose() }
}
