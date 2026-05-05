param(
    [Parameter(Mandatory = $true)]
    [string]$Serial,

    [switch]$RebootBeforeRun,

    [switch]$RecoverAfterRun,

    [int[]]$SnapshotSeconds = @(5, 20, 45),

    [int]$NetworkWaitTimeoutSeconds = 60,

    [int]$AdbCommandTimeoutSeconds = 30,

    [int]$AdbWaitForDeviceTimeoutSeconds = 120
)

$ErrorActionPreference = 'Stop'
$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path

Write-Host "Building and installing Android client..."
& powershell -NoProfile -ExecutionPolicy Bypass -File "$scriptDir\build-android-client.ps1" -Install
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}

Write-Host "Running controlled launch test..."
$snapshotLiteral = '@(' + ($SnapshotSeconds -join ',') + ')'
$launchCommand = @"
& '$scriptDir\pimax-controlled-launch-test.ps1' -Serial '$Serial' -SnapshotSeconds $snapshotLiteral -NetworkWaitTimeoutSeconds $NetworkWaitTimeoutSeconds -AdbCommandTimeoutSeconds $AdbCommandTimeoutSeconds -AdbWaitForDeviceTimeoutSeconds $AdbWaitForDeviceTimeoutSeconds -LeaveRunningWhenDisplayOff$(if ($RebootBeforeRun) { ' -RebootBeforeRun' } else { '' })$(if ($RecoverAfterRun) { ' -RecoverAfterRun' } else { '' })
"@

& powershell -NoProfile -ExecutionPolicy Bypass -Command $launchCommand

exit $LASTEXITCODE
