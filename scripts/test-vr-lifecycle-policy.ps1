Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
$classesDir = Join-Path $repoRoot ".tmp\java-policy-tests\vr-lifecycle\classes"
$sourcePath = Join-Path $repoRoot "android\java\com\pimax\alvr\client\VrLifecycleState.java"
$testPath = Join-Path $repoRoot "android\test\com\pimax\alvr\client\VrLifecycleStateTest.java"

if (Test-Path $classesDir) {
    Remove-Item -Recurse -Force -LiteralPath $classesDir
}
New-Item -ItemType Directory -Force -Path $classesDir | Out-Null

Write-Host "Compiling VR lifecycle policy regression tests..."
javac -encoding UTF-8 -Xlint:-options --release 8 -d $classesDir $sourcePath $testPath
if ($LASTEXITCODE -ne 0) {
    throw "javac failed with exit code $LASTEXITCODE"
}

Write-Host "Running VR lifecycle policy regression tests..."
java -cp $classesDir com.pimax.alvr.client.VrLifecycleStateTest
if ($LASTEXITCODE -ne 0) {
    throw "VrLifecycleStateTest failed with exit code $LASTEXITCODE"
}
