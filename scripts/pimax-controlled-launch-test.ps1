param(
    [string]$Serial,
    [string]$ArtifactRoot = ".tmp",
    [int[]]$SnapshotSeconds = @(5, 20, 45),
    [int]$Brightness = 135,
    [int]$NetworkWaitTimeoutSeconds = 90,
    [switch]$RebootBeforeRun,
    [switch]$RecoverAfterRun
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$packageName = "com.pimax.alvr.client"
$launchComponent = "com.pimax.alvr.client/com.pimax.alvr.client.VrRenderActivity"
$repoRoot = Split-Path -Parent $PSScriptRoot
$guardianHelper = Join-Path $PSScriptRoot "pimax-ensure-guardian-stationary.ps1"
$artifactRootPath = if ([System.IO.Path]::IsPathRooted($ArtifactRoot)) {
    $ArtifactRoot
} else {
    Join-Path $repoRoot $ArtifactRoot
}
$timestamp = Get-Date -Format "yyyy-MM-dd_HH-mm-ss"
$artifactDir = Join-Path $artifactRootPath "pimax_controlled_launch_$timestamp"
$script:AdbArgs = @()

if (-not [string]::IsNullOrWhiteSpace($Serial)) {
    $script:AdbArgs = @("-s", $Serial)
}

New-Item -ItemType Directory -Force -Path $artifactDir | Out-Null

$invalidSnapshotSeconds = @(
    $SnapshotSeconds |
        Where-Object { $_ -le 0 -or $_ -gt 600 }
)
if ($invalidSnapshotSeconds.Count -gt 0) {
    throw "Invalid -SnapshotSeconds value(s): $($invalidSnapshotSeconds -join ', '). Use space-separated values such as '-SnapshotSeconds 5 20 45'; PowerShell may parse '5,20,45' as a single grouped integer."
}

function Invoke-AdbCommand {
    param(
        [string]$Description,
        [string[]]$AdbCommandArgs,
        [switch]$AllowFailure
    )

    $previousErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $baseAdbArgs = $script:AdbArgs
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
    Set-Content -Encoding UTF8 -Path $path -Value $result.Output
    return $result
}

function Wait-For-BootCompleted {
    param(
        [int]$TimeoutSeconds = 240
    )

    Invoke-AdbCommand -Description "wait for adb device" -AdbCommandArgs @("wait-for-device") | Out-Null
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        Start-Sleep -Seconds 3
        $boot = Invoke-AdbCommand -Description "read boot_completed" -AdbCommandArgs @("shell", "getprop sys.boot_completed") -AllowFailure
        $bootText = ($boot.Output | Out-String).Trim()
        Write-Host "  boot_completed=$bootText"
        if ($bootText -eq "1") {
            return
        }
    } while ((Get-Date) -lt $deadline)

    throw "Timed out waiting for sys.boot_completed=1"
}

function Write-ProgressMarker {
    param(
        [string]$Message
    )

    $line = "$(Get-Date -Format o) $Message"
    Add-Content -Encoding UTF8 -Path (Join-Path $artifactDir "progress.txt") -Value $line
}

function Wait-For-NetworkReady {
    param(
        [int]$TimeoutSeconds = 90
    )

    Write-Host "Waiting for wlan0 network readiness..."
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    $attempt = 0
    do {
        $attempt++
        $network = Invoke-AdbCommand -Description "network readiness attempt $attempt" -AdbCommandArgs @("shell", "ip -o -4 addr show wlan0 2>/dev/null; ip route 2>/dev/null") -AllowFailure
        $networkText = ($network.Output | Out-String)
        $hasWlanIp = $networkText -match "\binet\s+\d+\.\d+\.\d+\.\d+/" -and $networkText -match "\bwlan0\b"
        $hasDefaultRoute = $networkText -match "(?m)^default\s+.*\bwlan0\b"

        if ($hasWlanIp) {
            $routeStatus = if ($hasDefaultRoute) { "default route present" } else { "no shell-visible default route" }
            Write-Host "  wlan0 has IPv4 after $attempt attempt(s); $routeStatus."
            $network.Output | Set-Content -Encoding UTF8 -Path (Join-Path $artifactDir "network-ready.txt")
            return $true
        }

        Write-Host "  wlan0 not ready yet (attempt $attempt)."
        Start-Sleep -Seconds 3
    } while ((Get-Date) -lt $deadline)

    Write-Warning "wlan0 did not report an IPv4 address within $TimeoutSeconds seconds; continuing so the artifact captures the failure mode."
    Save-AdbSnapshot -FileName "network-timeout.txt" -Description "network timeout state" -AdbCommandArgs @("shell", "ip -o -4 addr show wlan0 2>/dev/null; ip route 2>/dev/null; dumpsys wifi 2>/dev/null | grep -E 'Wi-Fi is|mNetworkInfo|mWifiInfo|SSID|BSSID|Supplicant|ClientModeImpl'") -AllowFailure | Out-Null
    return $false
}

function Initialize-HeadsetForRun {
    param(
        [string]$Reason
    )

    Write-Host "Preparing headset for run: $Reason"
    Invoke-AdbCommand -Description "enable stayon" -AdbCommandArgs @("shell", "svc power stayon true") | Out-Null
    Invoke-AdbCommand -Description "wake headset" -AdbCommandArgs @("shell", "input keyevent KEYCODE_WAKEUP") | Out-Null
    Start-Sleep -Milliseconds 700
    Invoke-AdbCommand -Description "dismiss keyguard" -AdbCommandArgs @("shell", "wm dismiss-keyguard") -AllowFailure | Out-Null
    Invoke-AdbCommand -Description "set brightness" -AdbCommandArgs @("shell", "settings put system screen_brightness $Brightness") -AllowFailure | Out-Null

    if (-not (Test-Path $guardianHelper)) {
        throw "Guardian helper not found at $guardianHelper"
    }

    $guardianArgs = @("-ExecutionPolicy", "Bypass", "-File", $guardianHelper, "-KeepAwake")
    if (-not [string]::IsNullOrWhiteSpace($Serial)) {
        $guardianArgs += @("-Serial", $Serial)
    }

    Write-Host "Reasserting persisted stationary Guardian..."
    $previousErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $guardianOutput = & powershell @guardianArgs 2>&1
        $guardianExit = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previousErrorActionPreference
    }
    $guardianOutput | Set-Content -Encoding UTF8 -Path (Join-Path $artifactDir "guardian-helper-$Reason.txt")
    if ($guardianExit -ne 0) {
        $joinedOutput = ($guardianOutput | Out-String).Trim()
        throw "Guardian helper failed during $Reason with exit code $guardianExit. $joinedOutput"
    }
}

function Stop-AppForCleanLaunch {
    param(
        [string]$PidFileName = "pid-before-clean-stop.txt",
        [string]$ForceStopFileName = "force-stop-before-launch.txt",
        [string]$SnapshotLabel = "after-clean-stop"
    )

    Write-Host "Ensuring $packageName is not already running before the launch..."
    $pidResult = Save-AdbSnapshot -FileName $PidFileName -Description "pid before clean stop" -AdbCommandArgs @("shell", "pidof $packageName") -AllowFailure
    $pidText = ($pidResult.Output | Out-String).Trim()
    if ([string]::IsNullOrWhiteSpace($pidText)) {
        Write-Host "  no existing $packageName process found."
        return
    }

    Write-Warning "$packageName was already running as pid $pidText; force-stopping before log capture and launch."
    Save-AdbSnapshot -FileName $ForceStopFileName -Description "force-stop before launch" -AdbCommandArgs @("shell", "am force-stop $packageName") -AllowFailure | Out-Null
    Start-Sleep -Seconds 2
    Invoke-AdbCommand -Description "wake headset after clean stop" -AdbCommandArgs @("shell", "input keyevent KEYCODE_WAKEUP") -AllowFailure | Out-Null
    Invoke-AdbCommand -Description "restore brightness after clean stop" -AdbCommandArgs @("shell", "settings put system screen_brightness $Brightness") -AllowFailure | Out-Null
    Save-DisplaySnapshot -Label $SnapshotLabel
    Write-DisplaySummary -Label $SnapshotLabel
}

function Wait-For-PackageExit {
    param(
        [int]$TimeoutSeconds = 15
    )

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        $pidResult = Invoke-AdbCommand -Description "poll pid for package exit" -AdbCommandArgs @("shell", "pidof $packageName") -AllowFailure
        $pidText = ($pidResult.Output | Out-String).Trim()
        if ([string]::IsNullOrWhiteSpace($pidText)) {
            return $true
        }
        Start-Sleep -Seconds 1
    } while ((Get-Date) -lt $deadline)

    return $false
}

function Save-DisplaySnapshot {
    param(
        [string]$Label
    )

    Write-ProgressMarker "snapshot $Label start"
    Save-AdbSnapshot -FileName "pid-$Label.txt" -Description "pid $Label" -AdbCommandArgs @("shell", "pidof $packageName") -AllowFailure | Out-Null
    Save-AdbSnapshot -FileName "power-$Label.txt" -Description "power $Label" -AdbCommandArgs @("shell", "dumpsys power") -AllowFailure | Out-Null
    Save-AdbSnapshot -FileName "display-$Label.txt" -Description "display $Label" -AdbCommandArgs @("shell", "dumpsys display") -AllowFailure | Out-Null
    Save-AdbSnapshot -FileName "activity-$Label.txt" -Description "activity $Label" -AdbCommandArgs @("shell", "dumpsys activity activities") -AllowFailure | Out-Null
    Save-AdbSnapshot -FileName "window-$Label.txt" -Description "window $Label" -AdbCommandArgs @("shell", "dumpsys window windows") -AllowFailure | Out-Null
    Save-AdbSnapshot -FileName "settings-$Label.txt" -Description "settings $Label" -AdbCommandArgs @("shell", 'echo brightness=$(settings get system screen_brightness); echo brightness_mode=$(settings get system screen_brightness_mode); echo guardian_effective=$(getprop pxr.vr.guardian.effective); echo pimax_guide=$(settings get system pimax_guide)') -AllowFailure | Out-Null

    $remoteScreenshot = "/sdcard/pimax_controlled_launch_$Label.png"
    $screencap = Invoke-AdbCommand -Description "screencap $Label" -AdbCommandArgs @("shell", "screencap -p $remoteScreenshot") -AllowFailure
    $screencap.Output | Set-Content -Encoding UTF8 -Path (Join-Path $artifactDir "screencap-$Label.txt")
    if ($screencap.ExitCode -eq 0) {
        Invoke-AdbCommand -Description "pull screencap $Label" -AdbCommandArgs @("pull", $remoteScreenshot, (Join-Path $artifactDir "screencap-$Label.png")) -AllowFailure | Out-Null
        Invoke-AdbCommand -Description "remove remote screencap $Label" -AdbCommandArgs @("shell", "rm $remoteScreenshot") -AllowFailure | Out-Null
    }
    Write-ProgressMarker "snapshot $Label end"
}

function Write-DisplaySummary {
    param(
        [string]$Label
    )

    Write-Host "Display summary ${Label}:"
    $powerPath = Join-Path $artifactDir "power-$Label.txt"
    $displayPath = Join-Path $artifactDir "display-$Label.txt"

    if (Test-Path $powerPath) {
        Get-Content $powerPath |
            Select-String -Pattern "mWakefulness=|mStayOn=|mHalInteractiveModeEnabled=|mWakeLockSummary=|mIsVrModeEnabled=|Display Power: state=" |
            ForEach-Object { Write-Host "  $($_.Line.Trim())" }
    }

    if (Test-Path $displayPath) {
        Get-Content $displayPath |
            Select-String -Pattern "mGlobalDisplayState=|mState=ON|mBrightness=|mScreenState=|mScreenBrightness=|mActualState=|mActualBacklight=" |
            Select-Object -First 12 |
            ForEach-Object { Write-Host "  $($_.Line.Trim())" }
    }
}

function Test-DisplayOff {
    param(
        [string]$Label
    )

    $powerPath = Join-Path $artifactDir "power-$Label.txt"
    $displayPath = Join-Path $artifactDir "display-$Label.txt"
    $powerText = if (Test-Path $powerPath) { Get-Content $powerPath | Out-String } else { "" }
    $displayText = if (Test-Path $displayPath) { Get-Content $displayPath | Out-String } else { "" }

    return (
        $powerText -match "Display Power: state=OFF" -or
        $powerText -match "mWakefulness=Asleep" -or
        $displayText -match "mGlobalDisplayState=OFF" -or
        $displayText -match "mActualBacklight=0"
    )
}

function Save-Logcat {
    param(
        [string]$Label = ""
    )

    Write-ProgressMarker "logcat $Label start"
    $suffix = if ([string]::IsNullOrWhiteSpace($Label)) { "" } else { "-$Label" }
    $result = Invoke-AdbCommand -Description "logcat dump" -AdbCommandArgs @("logcat", "-d", "*:V") -AllowFailure
    $logcatPath = Join-Path $artifactDir "logcat$suffix.txt"
    $result.Output | Set-Content -Encoding UTF8 -Path $logcatPath
    $filtered = Invoke-AdbCommand -Description "filtered logcat dump" -AdbCommandArgs @("logcat", "-d", "-v", "threadtime", "PimaxALVR:V", "PimaxALVRActivity:V", "PxrService:D", "PvrServiceClient:D", "pxr:V", "ActivityManager:I", "ActivityTaskManager:I", "AndroidRuntime:E", "DEBUG:E", "*:S") -AllowFailure
    $filtered.Output | Set-Content -Encoding UTF8 -Path (Join-Path $artifactDir "logcat-filtered$suffix.txt")
    $appOnly = Invoke-AdbCommand -Description "app logcat dump" -AdbCommandArgs @("logcat", "-d", "-v", "threadtime", "PimaxALVR:V", "PimaxALVRActivity:V", "AndroidRuntime:E", "DEBUG:E", "*:S") -AllowFailure
    $appOnly.Output | Set-Content -Encoding UTF8 -Path (Join-Path $artifactDir "logcat-app$suffix.txt")
    if (Test-Path $logcatPath) {
        Select-String -Path $logcatPath -Pattern "using application context|sxrShutdown|leaked|ServiceConnectionLeaked|IntentReceiverLeaked|StartVRMode|StopVRMode|Display Interrupt|SCREEN_OFF|screen_off|no fps|PimaxALVRActivity|PimaxALVR|sxrInitialize|sxrBeginXr|sxrEndXr" |
            Set-Content -Encoding UTF8 -Path (Join-Path $artifactDir "highlight$suffix.txt")
    }
    Write-ProgressMarker "logcat $Label end"
}

function Recover-HeadsetAfterRun {
    Write-ProgressMarker "recovery start"
    Write-Warning "Recovering headset after run by rebooting and reasserting Guardian."
    Save-AdbSnapshot -FileName "recover-reboot.txt" -Description "recover reboot" -AdbCommandArgs @("reboot") -AllowFailure | Out-Null
    Wait-For-BootCompleted
    Initialize-HeadsetForRun -Reason "post-recovery"
    Stop-AppForCleanLaunch -PidFileName "pid-post-recovery-before-clean-stop.txt" -ForceStopFileName "force-stop-post-recovery.txt" -SnapshotLabel "post-recovery-after-clean-stop"
    Save-DisplaySnapshot -Label "post-recovery"
    Write-DisplaySummary -Label "post-recovery"
    Write-ProgressMarker "recovery end"
}

$displayOff = $false
$logcatSaved = $false
$runError = $null

try {
    Write-ProgressMarker "script start"
    Write-Host "Controlled launch artifact: $artifactDir"
    Save-AdbSnapshot -FileName "devices.txt" -Description "adb devices" -AdbCommandArgs @("devices", "-l") | Out-Null

    if ($RebootBeforeRun) {
        Write-ProgressMarker "pre-run reboot start"
        Write-Host "Rebooting before run..."
        Save-AdbSnapshot -FileName "pre-run-reboot.txt" -Description "pre-run reboot" -AdbCommandArgs @("reboot") | Out-Null
        Wait-For-BootCompleted
        Write-ProgressMarker "pre-run reboot end"
    }

    Write-ProgressMarker "initialize pre-run start"
    Initialize-HeadsetForRun -Reason "pre-run"
    Write-ProgressMarker "initialize pre-run end"
    Stop-AppForCleanLaunch
    Write-ProgressMarker "network wait start"
    Wait-For-NetworkReady -TimeoutSeconds $NetworkWaitTimeoutSeconds | Out-Null
    Write-ProgressMarker "network wait end"
    Save-DisplaySnapshot -Label "before"
    Write-DisplaySummary -Label "before"

    Write-ProgressMarker "clear logcat start"
    Invoke-AdbCommand -Description "increase logcat buffer" -AdbCommandArgs @("logcat", "-G", "16M") -AllowFailure | Out-Null
    Invoke-AdbCommand -Description "clear logcat" -AdbCommandArgs @("logcat", "-c") -AllowFailure | Out-Null
    Write-ProgressMarker "clear logcat end"
    Write-ProgressMarker "am start start"
    Save-AdbSnapshot -FileName "am-start.txt" -Description "launch ALVR" -AdbCommandArgs @("shell", "am start -n $launchComponent") | Out-Null
    Write-ProgressMarker "am start end"

    $previousSecond = 0
    foreach ($second in ($SnapshotSeconds | Sort-Object -Unique)) {
        $sleepSeconds = [Math]::Max(0, $second - $previousSecond)
        if ($sleepSeconds -gt 0) {
            Write-ProgressMarker "sleep $sleepSeconds seconds before after-${second}s"
            Start-Sleep -Seconds $sleepSeconds
        }
        $previousSecond = $second
        $label = "after-${second}s"
        Save-DisplaySnapshot -Label $label
        Write-DisplaySummary -Label $label
        if (Test-DisplayOff -Label $label) {
            $displayOff = $true
            break
        }
    }

    if ($displayOff) {
        Write-Warning "Display appears OFF during controlled launch; force-stopping ALVR."
        Save-AdbSnapshot -FileName "force-stop.txt" -Description "force-stop ALVR" -AdbCommandArgs @("shell", "am force-stop $packageName") -AllowFailure | Out-Null
        Start-Sleep -Seconds 2
        Save-DisplaySnapshot -Label "after-force-stop"
        Write-DisplaySummary -Label "after-force-stop"
    } else {
        Write-Host "Display stayed ON through timed window; leaving app running."
        Save-DisplaySnapshot -Label "after-snapshots"
        Write-DisplaySummary -Label "after-snapshots"
    }

    Save-Logcat
    $logcatSaved = $true
} catch {
    $runError = $_
    Write-Warning "Controlled launch failed before completion: $($_.Exception.Message)"
    if (-not $logcatSaved) {
        try {
            Save-Logcat -Label "failure"
            $logcatSaved = $true
        } catch {
            Write-Warning "Failed to save failure logcat: $($_.Exception.Message)"
        }
    }
} finally {
    if ($RecoverAfterRun) {
        try {
            Recover-HeadsetAfterRun
            Save-Logcat -Label "post-recovery"
        } catch {
            Write-Warning "Post-run recovery failed: $($_.Exception.Message)"
        }
    }
}

Write-Host "Controlled launch complete. Artifact: $artifactDir"
if ($displayOff) {
    Write-Host "Result: display went OFF during launch."
} else {
    Write-Host "Result: display stayed ON through timed window."
}

if ($null -ne $runError) {
    throw $runError
}
