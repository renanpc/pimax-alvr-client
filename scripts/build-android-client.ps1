param(
    [ValidateSet("debug", "release")]
    [string]$Profile = "debug",
    [string]$Serial,
    [switch]$Install,
    [switch]$Launch,
    [int]$CargoBuildTimeoutSeconds = 3600,
    [int]$JavaCompileTimeoutSeconds = 300,
    [int]$DexBuildTimeoutSeconds = 300,
    [int]$ApkToolTimeoutSeconds = 120,
    [int]$ApkSignTimeoutSeconds = 120,
    [int]$AdbCommandTimeoutSeconds = 45,
    [int]$AdbWaitForDeviceTimeoutSeconds = 120
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Get-RequiredEnvPath {
    param(
        [string[]]$Names
    )

    foreach ($name in $Names) {
        $value = [Environment]::GetEnvironmentVariable($name)
        if (-not [string]::IsNullOrWhiteSpace($value)) {
            return $value
        }
    }

    throw "Missing required environment variable. Expected one of: $($Names -join ', ')"
}

function Get-LatestVersionDirectory {
    param(
        [string]$Root
    )

    $directory = Get-ChildItem -Path $Root -Directory |
        Sort-Object { [version]$_.Name } -Descending |
        Select-Object -First 1

    if ($null -eq $directory) {
        throw "No versioned directories found under $Root"
    }

    return $directory.FullName
}

function Invoke-ExternalCommand {
    param(
        [string]$Description,
        [string]$FilePath,
        [string[]]$Arguments = @(),
        [int]$TimeoutSeconds = 600,
        [switch]$AllowFailure,
        [switch]$NoEcho
    )

    if ($TimeoutSeconds -le 0) {
        throw "Invalid timeout for $Description`: $TimeoutSeconds seconds"
    }

    $previousErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        function Quote-CommandLineArgument {
            param([string]$Argument)

            if ($Argument -match '[\s"`]' ) {
                return '"' + ($Argument -replace '"', '\"') + '"'
            }

            return $Argument
        }

        $processInfo = New-Object System.Diagnostics.ProcessStartInfo
        $processInfo.FileName = $FilePath
        $processInfo.Arguments = ($Arguments | ForEach-Object { Quote-CommandLineArgument $_ }) -join ' '
        $processInfo.WorkingDirectory = (Get-Location).Path
        $processInfo.UseShellExecute = $false
        $processInfo.RedirectStandardOutput = $true
        $processInfo.RedirectStandardError = $true
        $processInfo.CreateNoWindow = $true

        $process = [System.Diagnostics.Process]::Start($processInfo)
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()

        if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
            try { $process.Kill($true) } catch { try { $process.Kill() } catch {} }
            $process.Dispose()
            $message = "$Description timed out after $TimeoutSeconds seconds: $FilePath $($Arguments -join ' ')"
            if ($AllowFailure) {
                Write-Warning $message
                return [pscustomobject]@{
                    ExitCode = 124
                    Output = @($message)
                }
            }

            throw $message
        }

        $process.WaitForExit() | Out-Null
        $stdout = $stdoutTask.GetAwaiter().GetResult()
        $stderr = $stderrTask.GetAwaiter().GetResult()
        $exitCode = $process.ExitCode
        $process.Dispose()
        $output = @()
        if (-not [string]::IsNullOrWhiteSpace($stdout)) { $output += $stdout -split "`r?`n" }
        if (-not [string]::IsNullOrWhiteSpace($stderr)) { $output += $stderr -split "`r?`n" }
    } finally {
        $ErrorActionPreference = $previousErrorActionPreference
    }

    if (-not $NoEcho -and $output.Count -gt 0) {
        $output | ForEach-Object { Write-Host $_ }
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

function Invoke-AdbCommand {
    param(
        [string]$Description,
        [string[]]$AdbCommandArgs,
        [int]$TimeoutSeconds = $AdbCommandTimeoutSeconds,
        [switch]$AllowFailure
    )

    $adbPath = (Get-Command adb -ErrorAction Stop).Source
    Invoke-ExternalCommand -Description $Description -FilePath $adbPath -Arguments ($script:AdbArgs + $AdbCommandArgs) -TimeoutSeconds $TimeoutSeconds -AllowFailure:$AllowFailure -NoEcho
}

function Invoke-AndroidDisplayWake {
    param(
        [string[]]$AdbArgs
    )

    Write-Host "Waking headset display before launch..."
    Invoke-AdbCommand -Description "adb wakeup" -AdbCommandArgs @("shell", "input", "keyevent", "KEYCODE_WAKEUP") | Out-Null

    Start-Sleep -Seconds 1

    Invoke-AdbCommand -Description "adb dismiss keyguard" -AdbCommandArgs @("shell", "wm", "dismiss-keyguard") -AllowFailure | Out-Null

    $powerState = Invoke-AdbCommand -Description "adb dumpsys power" -AdbCommandArgs @("shell", "dumpsys", "power")

    $powerState.Output |
        Select-String -Pattern "mWakefulness=|mHalInteractiveModeEnabled=|mLastSleepReason=|mIsVrModeEnabled=|Display Power: state=" |
            ForEach-Object { Write-Host "  $($_.Line.Trim())" }
}

function Write-AndroidDisplaySnapshot {
    param(
        [string[]]$AdbArgs,
        [string]$Label
    )

    Write-Host "Display state $Label..."

    $brightness = Invoke-AdbCommand -Description "adb screen_brightness" -AdbCommandArgs @("shell", "settings", "get", "system", "screen_brightness") -AllowFailure
    if ($brightness.ExitCode -eq 0) {
        Write-Host "  screen_brightness=$((($brightness.Output | Out-String).Trim()))"
    }

    $brightnessMode = Invoke-AdbCommand -Description "adb screen_brightness_mode" -AdbCommandArgs @("shell", "settings", "get", "system", "screen_brightness_mode") -AllowFailure
    if ($brightnessMode.ExitCode -eq 0) {
        Write-Host "  screen_brightness_mode=$((($brightnessMode.Output | Out-String).Trim()))"
    }

    $powerState = Invoke-AdbCommand -Description "adb dumpsys power" -AdbCommandArgs @("shell", "dumpsys", "power") -AllowFailure
    if ($powerState.ExitCode -eq 0) {
        $powerState.Output |
            Select-String -Pattern "mWakefulness=|mHalInteractiveModeEnabled=|mWakeLockSummary=|mIsVrModeEnabled=|Display Power: state=" |
            ForEach-Object { Write-Host "  $($_.Line.Trim())" }
    }

    $displayState = Invoke-AdbCommand -Description "adb dumpsys display" -AdbCommandArgs @("shell", "dumpsys", "display") -AllowFailure
    if ($displayState.ExitCode -eq 0) {
        $displayState.Output |
            Select-String -Pattern "mGlobalDisplayState=|mState=ON|mBrightness=|mScreenState=|mScreenBrightness=|mActualState=|mActualBacklight=" |
            Select-Object -First 12 |
            ForEach-Object { Write-Host "  $($_.Line.Trim())" }
    }
}

function Get-AndroidPackagePid {
    param(
        [string[]]$AdbArgs,
        [string]$PackageName
    )

    $pidOutput = Invoke-AdbCommand -Description "adb pidof $PackageName" -AdbCommandArgs @("shell", "pidof", $PackageName) -AllowFailure
    if ($pidOutput.ExitCode -ne 0) {
        return $null
    }

    $packagePid = ($pidOutput.Output -join " ").Trim()
    if ([string]::IsNullOrWhiteSpace($packagePid)) {
        return $null
    }

    return $packagePid
}

function Get-AndroidResumedActivitySummary {
    param(
        [string[]]$AdbArgs
    )

    $activityState = Invoke-AdbCommand -Description "adb dumpsys activity activities" -AdbCommandArgs @("shell", "dumpsys", "activity", "activities") -AllowFailure
    if ($activityState.ExitCode -ne 0) {
        Write-Warning "adb dumpsys activity failed with exit code $($activityState.ExitCode)"
        return $null
    }

    $resumedMatches = @(
        $activityState.Output |
            Select-String -Pattern "mResumedActivity:|topResumedActivity=|ResumedActivity:"
    )

    if ($resumedMatches.Count -eq 0) {
        return $null
    }

    return $resumedMatches[0].Line.Trim()
}

function Invoke-AndroidGracefulAppExit {
    param(
        [string[]]$AdbArgs,
        [string]$PackageName
    )

    $packagePid = Get-AndroidPackagePid -AdbArgs $AdbArgs -PackageName $PackageName
    if ([string]::IsNullOrWhiteSpace($packagePid)) {
        return
    }

    $resumedActivity = Get-AndroidResumedActivitySummary -AdbArgs $AdbArgs
    if (-not [string]::IsNullOrWhiteSpace($resumedActivity)) {
        Write-Host "Resumed activity before install: $resumedActivity"
    }

    if ([string]::IsNullOrWhiteSpace($resumedActivity) -or -not $resumedActivity.Contains($PackageName)) {
        Write-Warning "$PackageName is already running as pid $packagePid, but it is not the resumed activity; not sending BACK because it would affect the foreground shell."
        return
    }

    Write-Warning "$PackageName is already running as pid $packagePid and is foreground; sending BACK before install instead of force-stopping."
    $backResult = Invoke-AdbCommand -Description "adb BACK keyevent" -AdbCommandArgs @("shell", "input", "keyevent", "KEYCODE_BACK") -AllowFailure
    if ($backResult.ExitCode -ne 0) {
        Write-Warning "adb BACK keyevent failed with exit code $($backResult.ExitCode)"
        return
    }

    Start-Sleep -Seconds 2
    $packagePid = Get-AndroidPackagePid -AdbArgs $AdbArgs -PackageName $PackageName
    if (-not [string]::IsNullOrWhiteSpace($packagePid)) {
        Write-Warning "$PackageName is still running as pid $packagePid; continuing install without force-stop. The headset may need a reboot if the display remains dark."
    }
}

$repoRoot = Split-Path -Parent $PSScriptRoot
$androidHome = Get-RequiredEnvPath @("ANDROID_HOME", "ANDROID_SDK_ROOT")
$androidNdk = $null

foreach ($name in @("ANDROID_NDK_ROOT", "ANDROID_NDK_HOME", "NDK_HOME")) {
    $value = [Environment]::GetEnvironmentVariable($name)
    if (-not [string]::IsNullOrWhiteSpace($value)) {
        $androidNdk = $value
        break
    }
}

if ([string]::IsNullOrWhiteSpace($androidNdk)) {
    $ndkRoot = Join-Path $androidHome "ndk"
    if (-not (Test-Path $ndkRoot)) {
        throw "Android NDK not found. Set ANDROID_NDK_ROOT/ANDROID_NDK_HOME/NDK_HOME or install an NDK under $ndkRoot"
    }
    $androidNdk = Get-LatestVersionDirectory $ndkRoot
}

$env:ANDROID_HOME = $androidHome
Remove-Item Env:ANDROID_SDK_ROOT -ErrorAction SilentlyContinue
$env:ANDROID_NDK_ROOT = $androidNdk
$env:ANDROID_NDK_HOME = $androidNdk

$buildToolsDir = Get-LatestVersionDirectory (Join-Path $androidHome "build-tools")
$platformDir = Join-Path $androidHome "platforms\android-32"
$androidJar = Join-Path $platformDir "android.jar"
$javaSourceRoot = Join-Path $repoRoot "android\java"
$javaClassesDir = Join-Path $repoRoot ".tmp\java\classes"
$dexDir = Join-Path $repoRoot ".tmp\java\dex"
$dexPath = Join-Path $dexDir "classes.dex"
$apkDir = Join-Path $repoRoot "target\$Profile\apk"
$apkPath = Join-Path $apkDir "pimax-alvr-client.apk"
$unsignedApkPath = Join-Path $apkDir "pimax-alvr-client-with-java-unaligned.apk"
$alignedApkPath = Join-Path $apkDir "pimax-alvr-client-with-java-aligned.apk"
$packageName = "com.pimax.alvr.client"
$launchComponent = "com.pimax.alvr.client/com.pimax.alvr.client.VrRenderActivity"
$debugKeystore = Join-Path $env:USERPROFILE ".android\debug.keystore"
$javac = "javac"
$aapt = Join-Path $buildToolsDir "aapt.exe"
$d8 = Join-Path $buildToolsDir "d8.bat"
$zipalign = Join-Path $buildToolsDir "zipalign.exe"
$apksigner = Join-Path $buildToolsDir "apksigner.bat"
$adbArgs = @()

if (-not (Test-Path $androidJar)) {
    throw "Android platform jar not found at $androidJar"
}

if (-not (Test-Path $javaSourceRoot)) {
    throw "Java source directory not found at $javaSourceRoot"
}

if (-not (Test-Path $debugKeystore)) {
    throw "Debug keystore not found at $debugKeystore"
}

if (-not [string]::IsNullOrWhiteSpace($Serial)) {
    $adbArgs = @("-s", $Serial)
}

New-Item -ItemType Directory -Force -Path $javaClassesDir | Out-Null
New-Item -ItemType Directory -Force -Path $dexDir | Out-Null

Remove-Item -Recurse -Force $javaClassesDir
Remove-Item -Recurse -Force $dexDir
New-Item -ItemType Directory -Force -Path $javaClassesDir | Out-Null
New-Item -ItemType Directory -Force -Path $dexDir | Out-Null

Write-Host "Building native APK with cargo-apk ($Profile)..."
Push-Location $repoRoot
try {
    if ($Profile -eq "release") {
        Invoke-ExternalCommand -Description "cargo apk build --release" -FilePath "cargo" -Arguments @("apk", "build", "--release") -TimeoutSeconds $CargoBuildTimeoutSeconds | Out-Null
    } else {
        Invoke-ExternalCommand -Description "cargo apk build" -FilePath "cargo" -Arguments @("apk", "build") -TimeoutSeconds $CargoBuildTimeoutSeconds | Out-Null
    }

    Write-Host "Compiling Java NativeActivity wrapper..."
    $javaSources = @(Get-ChildItem -Path $javaSourceRoot -Recurse -Filter *.java | Select-Object -ExpandProperty FullName)
    if ($javaSources.Count -eq 0) {
        throw "No Java sources found under $javaSourceRoot"
    }

    Invoke-ExternalCommand -Description "javac compile" -FilePath $javac -Arguments (@("-encoding", "UTF-8", "-Xlint:none", "--release", "8", "-classpath", $androidJar, "-d", $javaClassesDir) + $javaSources) -TimeoutSeconds $JavaCompileTimeoutSeconds | Out-Null

    Write-Host "Building classes.dex..."
    $classFiles = @(Get-ChildItem -Path $javaClassesDir -Recurse -Filter *.class | Select-Object -ExpandProperty FullName)
    if ($classFiles.Count -eq 0) {
        throw "No compiled class files found under $javaClassesDir"
    }
    Invoke-ExternalCommand -Description "d8 dex build" -FilePath $d8 -Arguments (@("--lib", $androidJar, "--min-api", "26", "--output", $dexDir) + $classFiles) -TimeoutSeconds $DexBuildTimeoutSeconds | Out-Null
    if (-not (Test-Path $dexPath)) {
        throw "classes.dex was not produced at $dexPath"
    }

    if (-not (Test-Path $apkPath)) {
        throw "Base APK not found at $apkPath"
    }

    Copy-Item $apkPath $unsignedApkPath -Force

    Push-Location $dexDir
    try {
        Write-Host "Injecting classes.dex into APK..."
        Invoke-ExternalCommand -Description "aapt add classes.dex" -FilePath $aapt -Arguments @("add", $unsignedApkPath, "classes.dex") -TimeoutSeconds $ApkToolTimeoutSeconds | Out-Null
    } finally {
        Pop-Location
    }

    Write-Host "Aligning APK..."
    Invoke-ExternalCommand -Description "zipalign" -FilePath $zipalign -Arguments @("-f", "-v", "4", $unsignedApkPath, $alignedApkPath) -TimeoutSeconds $ApkToolTimeoutSeconds | Out-Null

    Write-Host "Signing final APK..."
    Invoke-ExternalCommand -Description "apksigner sign" -FilePath $apksigner -Arguments @("sign", "--ks", $debugKeystore, "--ks-pass", "pass:android", "--out", $apkPath, $alignedApkPath) -TimeoutSeconds $ApkSignTimeoutSeconds | Out-Null
    Invoke-ExternalCommand -Description "apksigner verify" -FilePath $apksigner -Arguments @("verify", $apkPath) -TimeoutSeconds $ApkSignTimeoutSeconds | Out-Null
} finally {
    Pop-Location
}

if ($Install) {
    Write-Host "Installing APK..."
    $remoteApkPath = "/data/local/tmp/pimax-alvr-client.apk"
    Invoke-AndroidGracefulAppExit -AdbArgs $adbArgs -PackageName $packageName
    # Uninstall previous version to ensure clean install (ignore failure if not installed)
    Write-Host "Uninstalling previous version (if present)..."
    $uninstall = Invoke-AdbCommand -Description "adb uninstall $packageName" -AdbCommandArgs @("uninstall", $packageName) -AllowFailure
    if ($uninstall.ExitCode -ne 0) {
        Write-Host "Package not installed yet; skipping uninstall."
    }
    Invoke-AdbCommand -Description "adb push apk" -AdbCommandArgs @("push", $apkPath, $remoteApkPath) | Out-Null
    Invoke-AdbCommand -Description "adb pm install" -AdbCommandArgs @("shell", "pm", "install", "-r", $remoteApkPath) | Out-Null
    Invoke-AdbCommand -Description "adb remove staged apk" -AdbCommandArgs @("shell", "rm", $remoteApkPath) | Out-Null
    Invoke-AdbCommand -Description "adb verify installed package" -AdbCommandArgs @("shell", "pm", "path", $packageName) | Out-Null
    $appops = Invoke-AdbCommand -Description "adb appops WRITE_SETTINGS" -AdbCommandArgs @("shell", "appops", "set", $packageName, "WRITE_SETTINGS", "allow") -AllowFailure
    if ($appops.ExitCode -ne 0) {
        Write-Warning "adb appops WRITE_SETTINGS failed with exit code $($appops.ExitCode); peak_refresh_rate requests may be denied."
    } else {
        Write-Host "Granted WRITE_SETTINGS app-op for peak_refresh_rate requests."
    }
    Write-AndroidDisplaySnapshot -AdbArgs $adbArgs -Label "after install"
}

if ($Launch) {
    Invoke-AndroidDisplayWake -AdbArgs $adbArgs
    Write-Host "Launching VrRenderActivity..."
    Invoke-AdbCommand -Description "adb launch" -AdbCommandArgs @("shell", "am", "start", "-n", $launchComponent) | Out-Null
    Start-Sleep -Seconds 2
    Write-AndroidDisplaySnapshot -AdbArgs $adbArgs -Label "after launch"
}

Write-Host "APK ready at $apkPath"
