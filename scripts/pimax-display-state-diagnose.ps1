param(
    [string]$Serial,
    [string]$ArtifactRoot = ".tmp",
    [string]$Label = "current"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
$artifactRootPath = if ([System.IO.Path]::IsPathRooted($ArtifactRoot)) {
    $ArtifactRoot
} else {
    Join-Path $repoRoot $ArtifactRoot
}
$timestamp = Get-Date -Format "yyyy-MM-dd_HH-mm-ss"
$artifactDir = Join-Path $artifactRootPath "pimax_display_state_$timestamp"
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

    $previousErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $output = & adb @script:AdbArgs @AdbCommandArgs 2>&1
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
    Set-Content -Encoding UTF8 -Path (Join-Path $artifactDir $FileName) -Value $result.Output
    return $result
}

function Get-FirstRegexGroup {
    param(
        [string]$Text,
        [string]$Pattern,
        [string]$Fallback = "UNKNOWN"
    )

    $match = [regex]::Match($Text, $Pattern, [System.Text.RegularExpressions.RegexOptions]::Multiline)
    if ($match.Success -and $match.Groups.Count -gt 1) {
        return $match.Groups[1].Value.Trim()
    }
    return $Fallback
}

function Analyze-Screencap {
    param(
        [string]$Path
    )

    if (-not (Test-Path $Path)) {
        return [ordered]@{
            classification = "NO_SCREENSHOT"
            avg_luma = ""
            dark_pct = ""
            bright_pct = ""
            interpretation = "No screenshot was captured."
        }
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
                "Android framebuffer has visible pixels."
            } else {
                "Android framebuffer is black or nearly black."
            }

            return [ordered]@{
                classification = $classification
                width = $width
                height = $height
                samples = $samples
                avg_luma = $averageLuma
                dark_pct = $darkPct
                bright_pct = $brightPct
                interpretation = $interpretation
            }
        } finally {
            $bitmap.Dispose()
        }
    } catch {
        return [ordered]@{
            classification = "ANALYSIS_FAILED"
            avg_luma = ""
            dark_pct = ""
            bright_pct = ""
            interpretation = $_.Exception.Message
        }
    }
}

Save-AdbSnapshot -FileName "devices.txt" -Description "adb devices" -AdbCommandArgs @("devices", "-l") | Out-Null
Save-AdbSnapshot -FileName "pid-alvr.txt" -Description "ALVR pid" -AdbCommandArgs @("shell", "pidof com.pimax.alvr.client") -AllowFailure | Out-Null
Save-AdbSnapshot -FileName "power-$Label.txt" -Description "power" -AdbCommandArgs @("shell", "dumpsys power") -AllowFailure | Out-Null
Save-AdbSnapshot -FileName "display-$Label.txt" -Description "display" -AdbCommandArgs @("shell", "dumpsys display") -AllowFailure | Out-Null
Save-AdbSnapshot -FileName "activity-$Label.txt" -Description "activity" -AdbCommandArgs @("shell", "dumpsys activity activities") -AllowFailure | Out-Null
Save-AdbSnapshot -FileName "window-$Label.txt" -Description "window" -AdbCommandArgs @("shell", "dumpsys window windows") -AllowFailure | Out-Null
Save-AdbSnapshot -FileName "settings-$Label.txt" -Description "settings" -AdbCommandArgs @("shell", 'echo brightness=$(settings get system screen_brightness); echo brightness_mode=$(settings get system screen_brightness_mode); echo eyechip_on=$(settings get system eyechip_on); echo screen_off_timeout=$(settings get system screen_off_timeout); echo pmx_pc_screen_off_timeout=$(settings get system pmx_pc_screen_off_timeout); echo sta_pm=$(getprop persist.sys.pmx.sta.pm.enable); echo disable_psensor=$(getprop persist.sys.pmx.dbg.disable.psensor)') -AllowFailure | Out-Null
Save-AdbSnapshot -FileName "panel-$Label.txt" -Description "physical panel/backlight state" -AdbCommandArgs @("shell", 'echo panel_requested=$(cat /sys/class/backlight/panel0-backlight/brightness 2>/dev/null); echo panel_actual=$(cat /sys/class/backlight/panel0-backlight/actual_brightness 2>/dev/null); echo panel_bl_power=$(cat /sys/class/backlight/panel0-backlight/bl_power 2>/dev/null); echo wled_actual=$(cat /sys/class/backlight/backlight/actual_brightness 2>/dev/null); echo drm_dsi_status=$(cat /sys/class/drm/card0-DSI-1/status 2>/dev/null); echo drm_dsi_enabled=$(cat /sys/class/drm/card0-DSI-1/enabled 2>/dev/null); echo drm_dsi_dpms=$(cat /sys/class/drm/card0-DSI-1/dpms 2>/dev/null); echo pc_dp_present=$(cat /sys/class/pc_switch/switch/dp_present 2>/dev/null); echo pc_mode_sw=$(cat /sys/class/pc_switch/switch/is_pc_mode_sw 2>/dev/null); echo panel_run_frame=$(cat /sys/class/pc_switch/switch/panel_run_frame 2>/dev/null); echo panel_type=$(cat /sys/class/pc_switch/switch/panel_type 2>/dev/null)') -AllowFailure | Out-Null
Save-AdbSnapshot -FileName "surface-list-$Label.txt" -Description "surface list" -AdbCommandArgs @("shell", "dumpsys SurfaceFlinger --list") -AllowFailure | Out-Null

$remoteScreenshot = "/sdcard/pimax_display_state_$Label.png"
$localScreenshot = Join-Path $artifactDir "screencap-$Label.png"
$screencap = Invoke-AdbCommand -Description "screencap" -AdbCommandArgs @("shell", "screencap -p $remoteScreenshot") -AllowFailure
$screencap.Output | Set-Content -Encoding UTF8 -Path (Join-Path $artifactDir "screencap-$Label.txt")
if ($screencap.ExitCode -eq 0) {
    $pull = Invoke-AdbCommand -Description "pull screencap" -AdbCommandArgs @("pull", $remoteScreenshot, $localScreenshot) -AllowFailure
    $pull.Output | Set-Content -Encoding UTF8 -Path (Join-Path $artifactDir "screencap-pull-$Label.txt")
    Invoke-AdbCommand -Description "remove remote screencap" -AdbCommandArgs @("shell", "rm $remoteScreenshot") -AllowFailure | Out-Null
}

$powerText = Get-Content (Join-Path $artifactDir "power-$Label.txt") | Out-String
$displayText = Get-Content (Join-Path $artifactDir "display-$Label.txt") | Out-String
$panelText = Get-Content (Join-Path $artifactDir "panel-$Label.txt") | Out-String
$pidText = (Get-Content (Join-Path $artifactDir "pid-alvr.txt") | Out-String).Trim()
$screen = Analyze-Screencap -Path $localScreenshot

$wakefulness = Get-FirstRegexGroup -Text $powerText -Pattern "mWakefulness=([^\r\n]+)"
$halInteractive = Get-FirstRegexGroup -Text $powerText -Pattern "mHalInteractiveModeEnabled=([^\r\n]+)"
$displayPower = Get-FirstRegexGroup -Text $powerText -Pattern "Display Power: state=([^\r\n]+)"
$lastSleepReason = Get-FirstRegexGroup -Text $powerText -Pattern "mLastSleepReason=([^\r\n]+)"
$globalDisplayState = Get-FirstRegexGroup -Text $displayText -Pattern "mGlobalDisplayState=([^\r\n]+)"
$builtInState = Get-FirstRegexGroup -Text $displayText -Pattern 'DisplayDeviceInfo\{"Built-in Screen".*? state ([A-Z]+),' 
$actualState = Get-FirstRegexGroup -Text $displayText -Pattern "mActualState=([^\r\n]+)"
$actualBacklight = Get-FirstRegexGroup -Text $displayText -Pattern "mActualBacklight=([^\r\n]+)"
$screenBrightness = Get-FirstRegexGroup -Text $displayText -Pattern "mScreenBrightness=([^\r\n]+)"
$panelRequested = Get-FirstRegexGroup -Text $panelText -Pattern "panel_requested=([^\r\n]*)"
$panelActual = Get-FirstRegexGroup -Text $panelText -Pattern "panel_actual=([^\r\n]*)"
$panelBlPower = Get-FirstRegexGroup -Text $panelText -Pattern "panel_bl_power=([^\r\n]*)"
$wledActual = Get-FirstRegexGroup -Text $panelText -Pattern "wled_actual=([^\r\n]*)"
$drmDsiStatus = Get-FirstRegexGroup -Text $panelText -Pattern "drm_dsi_status=([^\r\n]*)"
$drmDsiEnabled = Get-FirstRegexGroup -Text $panelText -Pattern "drm_dsi_enabled=([^\r\n]*)"
$drmDsiDpms = Get-FirstRegexGroup -Text $panelText -Pattern "drm_dsi_dpms=([^\r\n]*)"
$pcDpPresent = Get-FirstRegexGroup -Text $panelText -Pattern "pc_dp_present=([^\r\n]*)"
$pcModeSw = Get-FirstRegexGroup -Text $panelText -Pattern "pc_mode_sw=([^\r\n]*)"
$panelRunFrame = Get-FirstRegexGroup -Text $panelText -Pattern "panel_run_frame=([^\r\n]*)"
$panelType = Get-FirstRegexGroup -Text $panelText -Pattern "panel_type=([^\r\n]*)"

$panelOff = (
    $wakefulness -eq "Asleep" -or
    $displayPower -eq "OFF" -or
    $globalDisplayState -eq "OFF" -or
    $builtInState -eq "OFF" -or
    $actualState -eq "OFF" -or
    $actualBacklight -eq "0"
)
# On Crystal OG, panel_actual can remain 0 even while the display is visibly on.
# Prefer power/display state and explicit panel power/DPMS signals for off diagnosis.
$physicalPanelBacklightOff = (
    (-not [string]::IsNullOrWhiteSpace($panelBlPower) -and $panelBlPower -ne "0") -or
    $drmDsiDpms -eq "Off"
)
$framebufferBlack = $screen.classification -in @("BLACK_FRAMEBUFFER", "VERY_DARK_FRAMEBUFFER")

$diagnosis = if ($panelOff) {
    "PANEL_POWER_OFF"
} elseif ($physicalPanelBacklightOff) {
    "PHYSICAL_PANEL_BACKLIGHT_OFF"
} elseif ($framebufferBlack) {
    "DISPLAY_ON_FRAMEBUFFER_BLACK"
} elseif ($screen.classification -eq "NON_BLACK_FRAMEBUFFER") {
    "DISPLAY_ON_FRAMEBUFFER_VISIBLE"
} else {
    "UNKNOWN"
}

$summary = @(
    "artifact=$artifactDir",
    "diagnosis=$diagnosis",
    "alvr_pid=$pidText",
    "wakefulness=$wakefulness",
    "hal_interactive=$halInteractive",
    "display_power=$displayPower",
    "last_sleep_reason=$lastSleepReason",
    "global_display_state=$globalDisplayState",
    "built_in_state=$builtInState",
    "actual_state=$actualState",
    "actual_backlight=$actualBacklight",
    "screen_brightness=$screenBrightness",
    "panel_requested=$panelRequested",
    "panel_actual=$panelActual",
    "panel_bl_power=$panelBlPower",
    "wled_actual=$wledActual",
    "drm_dsi_status=$drmDsiStatus",
    "drm_dsi_enabled=$drmDsiEnabled",
    "drm_dsi_dpms=$drmDsiDpms",
    "pc_dp_present=$pcDpPresent",
    "pc_mode_sw=$pcModeSw",
    "panel_run_frame=$panelRunFrame",
    "panel_type=$panelType",
    "screencap_classification=$($screen.classification)",
    "screencap_avg_luma=$($screen.avg_luma)",
    "screencap_dark_pct=$($screen.dark_pct)",
    "screencap_bright_pct=$($screen.bright_pct)",
    "interpretation=$($screen.interpretation)"
)

$summaryPath = Join-Path $artifactDir "summary-$Label.txt"
$summary | Set-Content -Encoding UTF8 -Path $summaryPath

Write-Host "Pimax display state diagnosis:"
$summary | ForEach-Object { Write-Host "  $_" }
