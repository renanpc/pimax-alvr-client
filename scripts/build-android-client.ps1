param(
    [ValidateSet("debug", "release")]
    [string]$Profile = "debug",
    [string]$Serial,
    [switch]$Install,
    [switch]$Launch
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
        [scriptblock]$Command
    )

    & $Command
    if ($LASTEXITCODE -ne 0) {
        throw "$Description failed with exit code $LASTEXITCODE"
    }
}

function Invoke-AndroidDisplayWake {
    param(
        [string[]]$AdbArgs
    )

    Write-Host "Waking headset display before launch..."
    adb @AdbArgs shell input keyevent KEYCODE_WAKEUP
    if ($LASTEXITCODE -ne 0) {
        throw "adb wakeup failed with exit code $LASTEXITCODE"
    }

    Start-Sleep -Seconds 1

    adb @AdbArgs shell wm dismiss-keyguard | Out-Null
    if ($LASTEXITCODE -ne 0) {
        Write-Warning "adb dismiss-keyguard failed with exit code $LASTEXITCODE"
    }

    $powerState = adb @AdbArgs shell dumpsys power
    if ($LASTEXITCODE -ne 0) {
        throw "adb dumpsys power failed with exit code $LASTEXITCODE"
    }

    $powerState |
        Select-String -Pattern "mWakefulness=|mHalInteractiveModeEnabled=|mLastSleepReason=|mIsVrModeEnabled=|Display Power: state=" |
        ForEach-Object { Write-Host "  $($_.Line.Trim())" }
}

function Write-AndroidDisplaySnapshot {
    param(
        [string[]]$AdbArgs,
        [string]$Label
    )

    Write-Host "Display state $Label..."

    $brightness = adb @AdbArgs shell settings get system screen_brightness
    if ($LASTEXITCODE -ne 0) {
        Write-Warning "adb screen_brightness failed with exit code $LASTEXITCODE"
    } else {
        Write-Host "  screen_brightness=$($brightness.Trim())"
    }

    $brightnessMode = adb @AdbArgs shell settings get system screen_brightness_mode
    if ($LASTEXITCODE -ne 0) {
        Write-Warning "adb screen_brightness_mode failed with exit code $LASTEXITCODE"
    } else {
        Write-Host "  screen_brightness_mode=$($brightnessMode.Trim())"
    }

    $powerState = adb @AdbArgs shell dumpsys power
    if ($LASTEXITCODE -ne 0) {
        Write-Warning "adb dumpsys power failed with exit code $LASTEXITCODE"
    } else {
        $powerState |
            Select-String -Pattern "mWakefulness=|mHalInteractiveModeEnabled=|mWakeLockSummary=|mIsVrModeEnabled=|Display Power: state=" |
            ForEach-Object { Write-Host "  $($_.Line.Trim())" }
    }

    $displayState = adb @AdbArgs shell dumpsys display
    if ($LASTEXITCODE -ne 0) {
        Write-Warning "adb dumpsys display failed with exit code $LASTEXITCODE"
    } else {
        $displayState |
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

    $pidOutput = adb @AdbArgs shell pidof $PackageName
    if ($LASTEXITCODE -ne 0) {
        return $null
    }

    $packagePid = ($pidOutput -join " ").Trim()
    if ([string]::IsNullOrWhiteSpace($packagePid)) {
        return $null
    }

    return $packagePid
}

function Get-AndroidResumedActivitySummary {
    param(
        [string[]]$AdbArgs
    )

    $activityState = adb @AdbArgs shell dumpsys activity activities
    if ($LASTEXITCODE -ne 0) {
        Write-Warning "adb dumpsys activity failed with exit code $LASTEXITCODE"
        return $null
    }

    $resumedMatches = @(
        $activityState |
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
    adb @AdbArgs shell input keyevent KEYCODE_BACK
    if ($LASTEXITCODE -ne 0) {
        Write-Warning "adb BACK keyevent failed with exit code $LASTEXITCODE"
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
        Invoke-ExternalCommand "cargo apk build --release" { cargo apk build --release }
    } else {
        Invoke-ExternalCommand "cargo apk build" { cargo apk build }
    }

    Write-Host "Compiling Java NativeActivity wrapper..."
    $javaSources = @(Get-ChildItem -Path $javaSourceRoot -Recurse -Filter *.java | Select-Object -ExpandProperty FullName)
    if ($javaSources.Count -eq 0) {
        throw "No Java sources found under $javaSourceRoot"
    }

    Invoke-ExternalCommand "javac compile" {
        & $javac `
            -encoding UTF-8 `
            -Xlint:none `
            --release 8 `
            -classpath $androidJar `
            -d $javaClassesDir `
            $javaSources
    }

    Write-Host "Building classes.dex..."
    $classFiles = @(Get-ChildItem -Path $javaClassesDir -Recurse -Filter *.class | Select-Object -ExpandProperty FullName)
    if ($classFiles.Count -eq 0) {
        throw "No compiled class files found under $javaClassesDir"
    }
    Invoke-ExternalCommand "d8 dex build" {
        & $d8 `
            --lib $androidJar `
            --min-api 26 `
            --output $dexDir `
            $classFiles
    }
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
        Invoke-ExternalCommand "aapt add classes.dex" {
            & $aapt add $unsignedApkPath classes.dex
        }
    } finally {
        Pop-Location
    }

    Write-Host "Aligning APK..."
    Invoke-ExternalCommand "zipalign" {
        & $zipalign -f -v 4 $unsignedApkPath $alignedApkPath
    }

    Write-Host "Signing final APK..."
    Invoke-ExternalCommand "apksigner sign" {
        & $apksigner sign `
            --ks $debugKeystore `
            --ks-pass pass:android `
            --out $apkPath `
            $alignedApkPath
    }
    Invoke-ExternalCommand "apksigner verify" {
        & $apksigner verify $apkPath
    }
} finally {
    Pop-Location
}

if ($Install) {
    Write-Host "Installing APK..."
    $remoteApkPath = "/data/local/tmp/pimax-alvr-client.apk"
    Invoke-AndroidGracefulAppExit -AdbArgs $adbArgs -PackageName $packageName
    # Uninstall previous version to ensure clean install (ignore failure if not installed)
    Write-Host "Uninstalling previous version (if present)..."
    adb @adbArgs uninstall $packageName | Out-Null
    if ($LASTEXITCODE -ne 0) {
        Write-Host "Package not installed yet; skipping uninstall."
    }
    Invoke-ExternalCommand "adb push apk" {
        adb @adbArgs push $apkPath $remoteApkPath
    }
    Invoke-ExternalCommand "adb pm install" {
        adb @adbArgs shell pm install -r $remoteApkPath
    }
    Invoke-ExternalCommand "adb remove staged apk" {
        adb @adbArgs shell rm $remoteApkPath
    }
    Invoke-ExternalCommand "adb verify installed package" {
        adb @adbArgs shell pm path $packageName
    }
    adb @adbArgs shell appops set $packageName WRITE_SETTINGS allow
    if ($LASTEXITCODE -ne 0) {
        Write-Warning "adb appops WRITE_SETTINGS failed with exit code $LASTEXITCODE; peak_refresh_rate requests may be denied."
    } else {
        Write-Host "Granted WRITE_SETTINGS app-op for peak_refresh_rate requests."
    }
    Write-AndroidDisplaySnapshot -AdbArgs $adbArgs -Label "after install"
}

if ($Launch) {
    Invoke-AndroidDisplayWake -AdbArgs $adbArgs
    Write-Host "Launching VrRenderActivity..."
    Invoke-ExternalCommand "adb launch" {
        adb @adbArgs shell am start -n $launchComponent
    }
    Start-Sleep -Seconds 2
    Write-AndroidDisplaySnapshot -AdbArgs $adbArgs -Label "after launch"
}

Write-Host "APK ready at $apkPath"
