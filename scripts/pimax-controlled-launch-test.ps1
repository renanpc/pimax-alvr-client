param(
    [string]$Serial,
    [string]$ArtifactRoot = ".tmp",
    [int[]]$SnapshotSeconds = @(5, 20, 45, 120),
    [int]$Brightness = 135,
    [int]$NetworkWaitTimeoutSeconds = 90,
    [int]$SteamVRRestartWaitSeconds = 25,
    [switch]$RebootBeforeRun,
    [switch]$RecoverAfterRun,
    [switch]$SkipSteamVRRestart,
    [switch]$LeaveRunningWhenDisplayOff
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$packageName = "com.pimax.alvr.client"
$launchComponent = "com.pimax.alvr.client/com.pimax.alvr.client.VrRenderActivity"
$competingPackageNames = @(
    "com.pimax.vrstreaming"
)
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

function Get-SteamVRProcessSnapshot {
    $steamVrNames = @(
        "vrmonitor.exe",
        "vrserver.exe",
        "vrcompositor.exe",
        "vrdashboard.exe",
        "vrwebhelper.exe",
        "steamtours.exe",
        "vrstartup.exe"
    )

    @(Get-CimInstance Win32_Process | Where-Object { $steamVrNames -contains $_.Name })
}

function Save-SteamVRProcessSnapshot {
    param(
        [string]$Label
    )

    $processes = @(Get-SteamVRProcessSnapshot)
    $path = Join-Path $artifactDir "steamvr-processes-$Label.txt"
    if ($processes.Count -eq 0) {
        Set-Content -Encoding UTF8 -Path $path -Value "No SteamVR processes found."
        return $processes
    }

    $processes |
        Select-Object ProcessId, Name, ExecutablePath, CommandLine |
        Format-List |
        Out-String |
        Set-Content -Encoding UTF8 -Path $path
    return $processes
}

function Restart-SteamVR {
    if ($SkipSteamVRRestart) {
        Write-Host "Skipping SteamVR restart because -SkipSteamVRRestart was provided."
        Write-ProgressMarker "steamvr restart skipped"
        return
    }

    Write-Host "Restarting SteamVR before headset launch..."
    Write-ProgressMarker "steamvr restart start"
    $before = @(Save-SteamVRProcessSnapshot -Label "before-restart")
    $vrMonitorPath = $null
    if ($before.Count -gt 0) {
        $runningMonitor = $before |
            Where-Object { $_.Name -ieq "vrmonitor.exe" -and -not [string]::IsNullOrWhiteSpace($_.ExecutablePath) } |
            Select-Object -First 1
        if ($null -ne $runningMonitor) {
            $vrMonitorPath = $runningMonitor.ExecutablePath
        }
    }

    foreach ($process in $before) {
        Write-Host "  stopping $($process.Name) pid=$($process.ProcessId)"
        Stop-Process -Id $process.ProcessId -Force -ErrorAction SilentlyContinue
    }

    Start-Sleep -Seconds 4
    Save-SteamVRProcessSnapshot -Label "after-stop" | Out-Null

    $candidatePaths = @(
        $vrMonitorPath,
        "D:\Program Files (x86)\Steam\steamapps\common\SteamVR\bin\win64\vrmonitor.exe"
    )
    if (-not [string]::IsNullOrWhiteSpace(${env:ProgramFiles(x86)})) {
        $candidatePaths += Join-Path ${env:ProgramFiles(x86)} "Steam\steamapps\common\SteamVR\bin\win64\vrmonitor.exe"
    }
    if (-not [string]::IsNullOrWhiteSpace($env:ProgramFiles)) {
        $candidatePaths += Join-Path $env:ProgramFiles "Steam\steamapps\common\SteamVR\bin\win64\vrmonitor.exe"
    }
    $candidatePaths = $candidatePaths |
        Where-Object { -not [string]::IsNullOrWhiteSpace($_) } |
        Select-Object -Unique

    $started = $false
    foreach ($candidatePath in $candidatePaths) {
        if (Test-Path $candidatePath) {
            Write-Host "  starting SteamVR via $candidatePath"
            Start-Process -FilePath $candidatePath | Out-Null
            $started = $true
            break
        }
    }

    if (-not $started) {
        Write-Host "  vrmonitor.exe not found by path; starting SteamVR through the Steam URL handler."
        Start-Process "steam://rungameid/250820" | Out-Null
    }

    $deadline = (Get-Date).AddSeconds($SteamVRRestartWaitSeconds)
    do {
        Start-Sleep -Seconds 2
        $running = @(Get-SteamVRProcessSnapshot | Where-Object { $_.Name -in @("vrmonitor.exe", "vrserver.exe", "vrcompositor.exe") })
        if ($running.Count -gt 0) {
            Save-SteamVRProcessSnapshot -Label "after-restart" | Out-Null
            Write-Host "  SteamVR is running again ($($running.Name -join ', '))."
            Write-ProgressMarker "steamvr restart end"
            return
        }
    } while ((Get-Date) -lt $deadline)

    Save-SteamVRProcessSnapshot -Label "after-restart-timeout" | Out-Null
    Write-Warning "SteamVR did not report vrmonitor/vrserver/vrcompositor within $SteamVRRestartWaitSeconds seconds; continuing so the artifact captures the failure mode."
    Write-ProgressMarker "steamvr restart timeout"
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

function Stop-CompetingVrApps {
    Write-Host "Stopping competing Pimax VR apps before launch..."
    foreach ($competingPackageName in $competingPackageNames) {
        Save-AdbSnapshot `
            -FileName "pid-competing-$competingPackageName.txt" `
            -Description "pid for competing package $competingPackageName" `
            -AdbCommandArgs @("shell", "pidof $competingPackageName") `
            -AllowFailure | Out-Null

        Save-AdbSnapshot `
            -FileName "force-stop-competing-$competingPackageName.txt" `
            -Description "force-stop competing package $competingPackageName" `
            -AdbCommandArgs @("shell", "am force-stop $competingPackageName") `
            -AllowFailure | Out-Null
    }
}

function Grant-AppWriteSettings {
    Write-Host "Granting WRITE_SETTINGS app-op for $packageName..."
    $grant = Invoke-AdbCommand `
        -Description "grant WRITE_SETTINGS app-op" `
        -AdbCommandArgs @("shell", "appops set $packageName WRITE_SETTINGS allow") `
        -AllowFailure
    $grant.Output | Set-Content -Encoding UTF8 -Path (Join-Path $artifactDir "appops-write-settings-grant.txt")
    if ($grant.ExitCode -ne 0) {
        Write-Warning "WRITE_SETTINGS app-op grant failed; peak_refresh_rate and eyechip_on writes may be denied."
        return
    }

    Save-AdbSnapshot `
        -FileName "appops-write-settings-after-grant.txt" `
        -Description "appops WRITE_SETTINGS after grant" `
        -AdbCommandArgs @("shell", "appops get $packageName WRITE_SETTINGS") `
        -AllowFailure | Out-Null
}

function Set-PimaxBootProperties {
    param(
        [string]$Reason
    )

    Write-Host "Applying Pimax boot/runtime display properties: $Reason"
    Save-AdbSnapshot `
        -FileName "pimax-boot-properties-before-$Reason.txt" `
        -Description "Pimax boot properties before $Reason" `
        -AdbCommandArgs @("shell", 'echo sta_pm=$(getprop persist.sys.pmx.sta.pm.enable); echo psensor_gotosleep=$(getprop persist.sys.pmx.psensor.gotosleep); echo disable_psensor=$(getprop persist.sys.pmx.dbg.disable.psensor); echo pc_switch=$(getprop sys.pmx.pc.switch); echo sta_time=$(getprop persist.sys.pmx.sta.time); echo dim_screen=$(settings get system dim_screen); echo screen_off_timeout=$(settings get system screen_off_timeout); echo pmx_pc_screen_off_timeout=$(settings get system pmx_pc_screen_off_timeout)') `
        -AllowFailure | Out-Null

    Invoke-AdbCommand `
        -Description "disable transient Pimax PC-switch panel gate" `
        -AdbCommandArgs @("shell", "setprop sys.pmx.pc.switch 0") `
        -AllowFailure | Out-Null
    Invoke-AdbCommand `
        -Description "disable Pimax station power-management property" `
        -AdbCommandArgs @("shell", "setprop persist.sys.pmx.sta.pm.enable false") `
        -AllowFailure | Out-Null
    Invoke-AdbCommand `
        -Description "disable Pimax proximity/gyro sleep policy" `
        -AdbCommandArgs @("shell", "setprop persist.sys.pmx.psensor.gotosleep false") `
        -AllowFailure | Out-Null
    Invoke-AdbCommand `
        -Description "restore normal Pimax proximity state-machine" `
        -AdbCommandArgs @("shell", "setprop persist.sys.pmx.dbg.disable.psensor false") `
        -AllowFailure | Out-Null
    Invoke-AdbCommand `
        -Description "disable Android dim-screen setting" `
        -AdbCommandArgs @("shell", "settings put system dim_screen 0") `
        -AllowFailure | Out-Null
    Invoke-AdbCommand `
        -Description "extend Android screen-off timeout" `
        -AdbCommandArgs @("shell", "settings put system screen_off_timeout 2147483647") `
        -AllowFailure | Out-Null
    Invoke-AdbCommand `
        -Description "extend Pimax PC-mode screen-off timeout" `
        -AdbCommandArgs @("shell", "settings put system pmx_pc_screen_off_timeout 2147483647") `
        -AllowFailure | Out-Null

    Save-AdbSnapshot `
        -FileName "pimax-boot-properties-after-$Reason.txt" `
        -Description "Pimax boot properties after $Reason" `
        -AdbCommandArgs @("shell", 'echo sta_pm=$(getprop persist.sys.pmx.sta.pm.enable); echo psensor_gotosleep=$(getprop persist.sys.pmx.psensor.gotosleep); echo disable_psensor=$(getprop persist.sys.pmx.dbg.disable.psensor); echo pc_switch=$(getprop sys.pmx.pc.switch); echo sta_time=$(getprop persist.sys.pmx.sta.time); echo dim_screen=$(settings get system dim_screen); echo screen_off_timeout=$(settings get system screen_off_timeout); echo pmx_pc_screen_off_timeout=$(settings get system pmx_pc_screen_off_timeout)') `
        -AllowFailure | Out-Null
}

function Set-PimaxStreamingSettings {
    Write-Host "Applying Pimax streaming display settings..."
    Save-AdbSnapshot `
        -FileName "settings-before-streaming-tweaks.txt" `
        -Description "settings before streaming tweaks" `
        -AdbCommandArgs @("shell", 'echo eyechip_on=$(settings get system eyechip_on); echo peak_refresh_rate=$(settings get system peak_refresh_rate); echo dim_screen=$(settings get system dim_screen); echo screen_off_timeout=$(settings get system screen_off_timeout); echo pmx_pc_screen_off_timeout=$(settings get system pmx_pc_screen_off_timeout); echo sta_pm=$(getprop persist.sys.pmx.sta.pm.enable); echo psensor_gotosleep=$(getprop persist.sys.pmx.psensor.gotosleep); echo disable_psensor=$(getprop persist.sys.pmx.dbg.disable.psensor); echo pc_switch=$(getprop sys.pmx.pc.switch)') `
        -AllowFailure | Out-Null

    Set-PimaxBootProperties -Reason "streaming-tweaks"
    Invoke-AdbCommand `
        -Description "disable Pimax eyechip suspend policy" `
        -AdbCommandArgs @("shell", "settings put system eyechip_on 0") `
        -AllowFailure | Out-Null
    Invoke-AdbCommand `
        -Description "set peak refresh rate" `
        -AdbCommandArgs @("shell", "settings put system peak_refresh_rate 90") `
        -AllowFailure | Out-Null

    Save-AdbSnapshot `
        -FileName "settings-after-streaming-tweaks.txt" `
        -Description "settings after streaming tweaks" `
        -AdbCommandArgs @("shell", 'echo eyechip_on=$(settings get system eyechip_on); echo peak_refresh_rate=$(settings get system peak_refresh_rate); echo dim_screen=$(settings get system dim_screen); echo screen_off_timeout=$(settings get system screen_off_timeout); echo pmx_pc_screen_off_timeout=$(settings get system pmx_pc_screen_off_timeout); echo sta_pm=$(getprop persist.sys.pmx.sta.pm.enable); echo psensor_gotosleep=$(getprop persist.sys.pmx.psensor.gotosleep); echo disable_psensor=$(getprop persist.sys.pmx.dbg.disable.psensor); echo pc_switch=$(getprop sys.pmx.pc.switch)') `
        -AllowFailure | Out-Null
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

function Analyze-Screencap {
    param(
        [string]$Label,
        [string]$Path
    )

    $analysisPath = Join-Path $artifactDir "screencap-analysis-$Label.txt"
    if (-not (Test-Path $Path)) {
        Set-Content -Encoding UTF8 -Path $analysisPath -Value "classification=NO_SCREENSHOT"
        return
    }

    try {
        Add-Type -AssemblyName System.Drawing
        $bitmap = [System.Drawing.Bitmap]::new($Path)
        try {
            $width = $bitmap.Width
            $height = $bitmap.Height
            $stepX = [Math]::Max(1, [int][Math]::Floor($width / 96))
            $stepY = [Math]::Max(1, [int][Math]::Floor($height / 96))
            $samples = 0
            $darkSamples = 0
            $brightSamples = 0
            [double]$totalLuma = 0

            for ($y = 0; $y -lt $height; $y += $stepY) {
                for ($x = 0; $x -lt $width; $x += $stepX) {
                    $pixel = $bitmap.GetPixel($x, $y)
                    $luma = (0.2126 * $pixel.R) + (0.7152 * $pixel.G) + (0.0722 * $pixel.B)
                    $totalLuma += $luma
                    $samples++
                    if ($luma -lt 8) {
                        $darkSamples++
                    }
                    if ($luma -gt 32) {
                        $brightSamples++
                    }
                }
            }

            $averageLuma = [Math]::Round($totalLuma / [Math]::Max(1, $samples), 2)
            $darkPct = [Math]::Round(($darkSamples * 100.0) / [Math]::Max(1, $samples), 2)
            $brightPct = [Math]::Round(($brightSamples * 100.0) / [Math]::Max(1, $samples), 2)
            $classification = if ($averageLuma -lt 8 -and $brightPct -lt 0.5) {
                "BLACK_FRAMEBUFFER"
            } elseif ($averageLuma -lt 20 -and $brightPct -lt 5) {
                "VERY_DARK_FRAMEBUFFER"
            } else {
                "NON_BLACK_FRAMEBUFFER"
            }
            $interpretation = if ($classification -eq "NON_BLACK_FRAMEBUFFER") {
                "Android framebuffer has visible pixels. If the headset lenses are black, suspect panel/compositor/presentation path rather than app rendering."
            } else {
                "Android framebuffer itself is black or nearly black."
            }

            @(
                "classification=$classification",
                "width=$width",
                "height=$height",
                "samples=$samples",
                "avg_luma=$averageLuma",
                "dark_pct=$darkPct",
                "bright_pct=$brightPct",
                "interpretation=$interpretation"
            ) | Set-Content -Encoding UTF8 -Path $analysisPath
        } finally {
            $bitmap.Dispose()
        }
    } catch {
        @(
            "classification=ANALYSIS_FAILED",
            "error=$($_.Exception.Message)"
        ) | Set-Content -Encoding UTF8 -Path $analysisPath
    }
}

function Save-DisplaySnapshot {
    param(
        [string]$Label
    )

    Write-ProgressMarker "snapshot $Label start"
    Save-AdbSnapshot -FileName "pid-$Label.txt" -Description "pid $Label" -AdbCommandArgs @("shell", "pidof $packageName") -AllowFailure | Out-Null
    Save-AdbSnapshot -FileName "power-$Label.txt" -Description "power $Label" -AdbCommandArgs @("shell", "dumpsys power") -AllowFailure | Out-Null
    Save-AdbSnapshot -FileName "display-$Label.txt" -Description "display $Label" -AdbCommandArgs @("shell", "dumpsys display") -AllowFailure | Out-Null
    Save-AdbSnapshot -FileName "surfaceflinger-$Label.txt" -Description "surfaceflinger $Label" -AdbCommandArgs @("shell", "dumpsys SurfaceFlinger") -AllowFailure | Out-Null
    Save-AdbSnapshot -FileName "activity-$Label.txt" -Description "activity $Label" -AdbCommandArgs @("shell", "dumpsys activity activities") -AllowFailure | Out-Null
    Save-AdbSnapshot -FileName "window-$Label.txt" -Description "window $Label" -AdbCommandArgs @("shell", "dumpsys window windows") -AllowFailure | Out-Null
    Save-AdbSnapshot -FileName "settings-$Label.txt" -Description "settings $Label" -AdbCommandArgs @("shell", 'echo brightness=$(settings get system screen_brightness); echo brightness_mode=$(settings get system screen_brightness_mode); echo eyechip_on=$(settings get system eyechip_on); echo dim_screen=$(settings get system dim_screen); echo screen_off_timeout=$(settings get system screen_off_timeout); echo pmx_pc_screen_off_timeout=$(settings get system pmx_pc_screen_off_timeout); echo sta_pm=$(getprop persist.sys.pmx.sta.pm.enable); echo psensor_gotosleep=$(getprop persist.sys.pmx.psensor.gotosleep); echo disable_psensor=$(getprop persist.sys.pmx.dbg.disable.psensor); echo pc_switch=$(getprop sys.pmx.pc.switch); echo guardian_effective=$(getprop pxr.vr.guardian.effective); echo pimax_guide=$(settings get system pimax_guide)') -AllowFailure | Out-Null
    Save-AdbSnapshot -FileName "panel-$Label.txt" -Description "panel $Label" -AdbCommandArgs @("shell", 'echo panel_requested=$(cat /sys/class/backlight/panel0-backlight/brightness 2>/dev/null); echo panel_actual=$(cat /sys/class/backlight/panel0-backlight/actual_brightness 2>/dev/null); echo panel_bl_power=$(cat /sys/class/backlight/panel0-backlight/bl_power 2>/dev/null); echo wled_actual=$(cat /sys/class/backlight/backlight/actual_brightness 2>/dev/null); echo drm_dsi_status=$(cat /sys/class/drm/card0-DSI-1/status 2>/dev/null); echo drm_dsi_enabled=$(cat /sys/class/drm/card0-DSI-1/enabled 2>/dev/null); echo drm_dsi_dpms=$(cat /sys/class/drm/card0-DSI-1/dpms 2>/dev/null); echo pc_dp_present=$(cat /sys/class/pc_switch/switch/dp_present 2>/dev/null); echo pc_mode_sw=$(cat /sys/class/pc_switch/switch/is_pc_mode_sw 2>/dev/null); echo panel_run_frame=$(cat /sys/class/pc_switch/switch/panel_run_frame 2>/dev/null); echo panel_type=$(cat /sys/class/pc_switch/switch/panel_type 2>/dev/null)') -AllowFailure | Out-Null

    $remoteScreenshot = "/sdcard/pimax_controlled_launch_$Label.png"
    $localScreenshot = Join-Path $artifactDir "screencap-$Label.png"
    $screencap = Invoke-AdbCommand -Description "screencap $Label" -AdbCommandArgs @("shell", "screencap -p $remoteScreenshot") -AllowFailure
    $screencap.Output | Set-Content -Encoding UTF8 -Path (Join-Path $artifactDir "screencap-$Label.txt")
    if ($screencap.ExitCode -eq 0) {
        $pull = Invoke-AdbCommand -Description "pull screencap $Label" -AdbCommandArgs @("pull", $remoteScreenshot, $localScreenshot) -AllowFailure
        $pull.Output | Set-Content -Encoding UTF8 -Path (Join-Path $artifactDir "screencap-pull-$Label.txt")
        if ($pull.ExitCode -eq 0) {
            Analyze-Screencap -Label $Label -Path $localScreenshot
        }
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

    $panelPath = Join-Path $artifactDir "panel-$Label.txt"
    if (Test-Path $panelPath) {
        Get-Content $panelPath |
            Select-String -Pattern "^panel_requested=|^panel_actual=|^panel_bl_power=|^wled_actual=|^drm_dsi_status=|^drm_dsi_enabled=|^drm_dsi_dpms=|^pc_dp_present=|^pc_mode_sw=" |
            ForEach-Object { Write-Host "  physical $($_.Line.Trim())" }
    }

    $analysisPath = Join-Path $artifactDir "screencap-analysis-$Label.txt"
    if (Test-Path $analysisPath) {
        Get-Content $analysisPath |
            Select-String -Pattern "^classification=|^avg_luma=|^dark_pct=|^bright_pct=|^interpretation=" |
            ForEach-Object { Write-Host "  framebuffer $($_.Line.Trim())" }
    }
}

function Test-DisplayOff {
    param(
        [string]$Label
    )

    $powerPath = Join-Path $artifactDir "power-$Label.txt"
    $displayPath = Join-Path $artifactDir "display-$Label.txt"
    $panelPath = Join-Path $artifactDir "panel-$Label.txt"
    $powerText = if (Test-Path $powerPath) { Get-Content $powerPath | Out-String } else { "" }
    $displayText = if (Test-Path $displayPath) { Get-Content $displayPath | Out-String } else { "" }
    $panelText = if (Test-Path $panelPath) { Get-Content $panelPath | Out-String } else { "" }

    return (
        $powerText -match "Display Power: state=OFF" -or
        $powerText -match "mWakefulness=Asleep" -or
        $displayText -match "mGlobalDisplayState=OFF" -or
        $displayText -match "mActualBacklight=0" -or
        $panelText -match "(?m)^panel_actual=0$"
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
        Set-PimaxBootProperties -Reason "before-pre-run-reboot"
        Write-Host "Rebooting before run..."
        Save-AdbSnapshot -FileName "pre-run-reboot.txt" -Description "pre-run reboot" -AdbCommandArgs @("reboot") | Out-Null
        Wait-For-BootCompleted
        Write-ProgressMarker "pre-run reboot end"
    }

    Write-ProgressMarker "initialize pre-run start"
    Initialize-HeadsetForRun -Reason "pre-run"
    Write-ProgressMarker "initialize pre-run end"
    Stop-AppForCleanLaunch
    Stop-CompetingVrApps
    Grant-AppWriteSettings
    Set-PimaxStreamingSettings
    Write-ProgressMarker "network wait start"
    Wait-For-NetworkReady -TimeoutSeconds $NetworkWaitTimeoutSeconds | Out-Null
    Write-ProgressMarker "network wait end"
    Restart-SteamVR
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

    if ($displayOff -and $LeaveRunningWhenDisplayOff) {
        Write-Warning "Display appears OFF during controlled launch; leaving ALVR running because -LeaveRunningWhenDisplayOff was provided."
        Save-DisplaySnapshot -Label "after-display-off-left-running"
        Write-DisplaySummary -Label "after-display-off-left-running"
    } elseif ($displayOff) {
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
