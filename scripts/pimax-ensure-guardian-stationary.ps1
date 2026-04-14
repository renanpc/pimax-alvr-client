param(
    [string]$Serial,
    [string]$ArtifactRoot = ".tmp",
    [switch]$KeepAwake,
    [switch]$LaunchGuardianIfMissing,
    [switch]$LaunchVrShell
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$guardianPath = "/mnt/vendor/persist/pimax/system/pxr_guardian.txt"
$repoRoot = Split-Path -Parent $PSScriptRoot
$artifactRootPath = if ([System.IO.Path]::IsPathRooted($ArtifactRoot)) {
    $ArtifactRoot
} else {
    Join-Path $repoRoot $ArtifactRoot
}
$timestamp = Get-Date -Format "yyyy-MM-dd_HH-mm-ss"
$artifactDir = Join-Path $artifactRootPath "pimax_guardian_stationary_$timestamp"
$script:AdbArgs = @()

if (-not [string]::IsNullOrWhiteSpace($Serial)) {
    $script:AdbArgs = @("-s", $Serial)
}

New-Item -ItemType Directory -Force -Path $artifactDir | Out-Null

function Invoke-AdbCommand {
    param(
        [string]$Description,
        [string[]]$AdbCommandArgs,
        [switch]$AllowFailure
    )

    $baseAdbArgs = $script:AdbArgs
    $previousErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $output = & adb @baseAdbArgs @AdbCommandArgs 2>&1
        $exitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previousErrorActionPreference
    }

    if ($exitCode -ne 0 -and -not $AllowFailure) {
        $joinedOutput = ($output | Out-String).Trim()
        throw "$Description failed with exit code $exitCode. $joinedOutput"
    }

    return [pscustomobject]@{
        ExitCode = $exitCode
        Output = $output
    }
}

function Save-AdbSnapshot {
    param(
        [string]$FileName,
        [string]$Description,
        [string[]]$AdbCommandArgs,
        [switch]$AllowFailure
    )

    $result = Invoke-AdbCommand -Description $Description -AdbCommandArgs $AdbCommandArgs -AllowFailure:$AllowFailure
    $path = Join-Path $artifactDir $FileName
    $result.Output | Set-Content -Encoding UTF8 -Path $path
    return $result
}

function Get-GuardianValue {
    param(
        [string]$GuardianText,
        [string]$Key
    )

    $match = [regex]::Match($GuardianText, "(?m)^$([regex]::Escape($Key))\s*=\s*(.+?)\s*$")
    if (-not $match.Success) {
        return $null
    }

    return $match.Groups[1].Value.Trim()
}

Write-Host "Capturing Guardian/display baseline in $artifactDir"
Save-AdbSnapshot -FileName "devices.txt" -Description "adb devices" -AdbCommandArgs @("devices", "-l") | Out-Null
Save-AdbSnapshot -FileName "activity-before.txt" -Description "activity before" -AdbCommandArgs @("shell", "dumpsys activity activities") -AllowFailure | Out-Null
Save-AdbSnapshot -FileName "power-before.txt" -Description "power before" -AdbCommandArgs @("shell", "dumpsys power") -AllowFailure | Out-Null
Save-AdbSnapshot -FileName "display-before.txt" -Description "display before" -AdbCommandArgs @("shell", "dumpsys display") -AllowFailure | Out-Null

Write-Host "Waking headset display..."
Invoke-AdbCommand -Description "wake headset" -AdbCommandArgs @("shell", "input keyevent KEYCODE_WAKEUP") | Out-Null
Start-Sleep -Milliseconds 500
Invoke-AdbCommand -Description "dismiss keyguard" -AdbCommandArgs @("shell", "wm dismiss-keyguard") -AllowFailure | Out-Null

if ($KeepAwake) {
    Write-Host "Keeping display awake while testing..."
    Invoke-AdbCommand -Description "enable stayon" -AdbCommandArgs @("shell", "svc power stayon true") -AllowFailure | Out-Null
}

Write-Host "Reading persisted Guardian boundary..."
$statResult = Save-AdbSnapshot -FileName "guardian-stat.txt" -Description "guardian stat" -AdbCommandArgs @("shell", "stat $guardianPath 2>&1") -AllowFailure
$guardianResult = Save-AdbSnapshot -FileName "pxr_guardian.txt" -Description "guardian cat" -AdbCommandArgs @("shell", "cat $guardianPath 2>&1") -AllowFailure
$guardianText = ($guardianResult.Output | Out-String)
$hasGuardianFile = $statResult.ExitCode -eq 0 -and $guardianResult.ExitCode -eq 0 -and -not [string]::IsNullOrWhiteSpace($guardianText)

if (-not $hasGuardianFile) {
    Write-Warning "No readable Guardian file was found at $guardianPath."
    if ($LaunchGuardianIfMissing) {
        Write-Warning "Launching stock Guardian so the boundary can be created in-headset."
        Invoke-AdbCommand -Description "launch Guardian" -AdbCommandArgs @("shell", "am start -n com.pimax.vrguardian/.activity.MainUnityActivity --es action guide --es Starter codex_guardian_helper") | Out-Null
        Start-Sleep -Seconds 2
        Save-AdbSnapshot -FileName "activity-after-guardian-launch.txt" -Description "activity after Guardian launch" -AdbCommandArgs @("shell", "dumpsys activity activities") -AllowFailure | Out-Null
        Save-AdbSnapshot -FileName "power-after-guardian-launch.txt" -Description "power after Guardian launch" -AdbCommandArgs @("shell", "dumpsys power") -AllowFailure | Out-Null
        Save-AdbSnapshot -FileName "display-after-guardian-launch.txt" -Description "display after Guardian launch" -AdbCommandArgs @("shell", "dumpsys display") -AllowFailure | Out-Null
        throw "Guardian needs in-headset confirmation. Finish stationary setup, then rerun this script."
    }

    throw "Guardian file is missing or unreadable. Rerun with -LaunchGuardianIfMissing to open stock Guardian."
}

$geometry = Get-GuardianValue -GuardianText $guardianText -Key "gGeometry"
$sizeType = Get-GuardianValue -GuardianText $guardianText -Key "cSizeType"
$groundY = Get-GuardianValue -GuardianText $guardianText -Key "cGroundY"
$boundaryType = Get-GuardianValue -GuardianText $guardianText -Key "gBoundaryType"
$poseType = Get-GuardianValue -GuardianText $guardianText -Key "gPoseType"

if ([string]::IsNullOrWhiteSpace($geometry)) {
    throw "Guardian file exists, but does not contain gGeometry."
}

$geometryValues = @(
    $geometry -split "," |
        ForEach-Object { $_.Trim() } |
        Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
)

if ($geometryValues.Count -lt 9 -or ($geometryValues.Count % 3) -ne 0) {
    throw "Guardian gGeometry has $($geometryValues.Count) values; expected a multiple of 3 with at least 3 points."
}

$pointCount = [int]($geometryValues.Count / 3)
Write-Host "Guardian boundary looks usable: points=$pointCount cSizeType=$sizeType cGroundY=$groundY gBoundaryType=$boundaryType gPoseType=$poseType"

Write-Host "Re-asserting Pimax Guardian runtime state..."
Invoke-AdbCommand -Description "set guardian effective" -AdbCommandArgs @("shell", "setprop pxr.vr.guardian.effective 1") | Out-Null
Invoke-AdbCommand -Description "set guide complete" -AdbCommandArgs @("shell", "settings put system pimax_guide 1") -AllowFailure | Out-Null

if ($LaunchVrShell) {
    Write-Host "Launching VrShell with guide_vrguardian_done action..."
    Invoke-AdbCommand -Description "launch VrShell" -AdbCommandArgs @("shell", "am start -n com.pimax.vrshell/.activity.MainUnityActivity --es action guide_vrguardian_done") -AllowFailure | Out-Null
}

Save-AdbSnapshot -FileName "guardian-state-after.txt" -Description "guardian state after" -AdbCommandArgs @("shell", 'echo pxr.vr.guardian.effective=$(getprop pxr.vr.guardian.effective); echo pimax_guide=$(settings get system pimax_guide); settings get system screen_brightness; settings get system screen_brightness_mode') -AllowFailure | Out-Null
Save-AdbSnapshot -FileName "activity-after.txt" -Description "activity after" -AdbCommandArgs @("shell", "dumpsys activity activities") -AllowFailure | Out-Null
Save-AdbSnapshot -FileName "power-after.txt" -Description "power after" -AdbCommandArgs @("shell", "dumpsys power") -AllowFailure | Out-Null
Save-AdbSnapshot -FileName "display-after.txt" -Description "display after" -AdbCommandArgs @("shell", "dumpsys display") -AllowFailure | Out-Null

$stateAfter = Get-Content -Path (Join-Path $artifactDir "guardian-state-after.txt")
Write-Host "Guardian state after:"
$stateAfter | ForEach-Object { Write-Host "  $($_.Trim())" }

$powerAfter = Get-Content -Path (Join-Path $artifactDir "power-after.txt")
$displayAfter = Get-Content -Path (Join-Path $artifactDir "display-after.txt")
Write-Host "Display state after:"
$powerAfter |
    Select-String -Pattern "mWakefulness=|mHalInteractiveModeEnabled=|Display Power: state=" |
    ForEach-Object { Write-Host "  $($_.Line.Trim())" }
$displayAfter |
    Select-String -Pattern "mGlobalDisplayState=|mState=ON|mBrightness=|mScreenBrightness=|mActualBacklight=" |
    Select-Object -First 12 |
    ForEach-Object { Write-Host "  $($_.Line.Trim())" }

Write-Host "Guardian stationary ensure complete. Artifact: $artifactDir"
