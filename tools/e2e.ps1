param(
    [Parameter(Mandatory = $true)]
    [string]$Javac,

    [string]$Notlin = (Join-Path (Split-Path -Parent $PSScriptRoot) 'target/debug/notlin.exe')
)

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
$samplesDir = Join-Path $repoRoot 'samples'
$annotationsJar = Join-Path $repoRoot 'vendor/jetbrains-annotations.jar'
$outputRoot = Join-Path $repoRoot (Join-Path 'target/javac-e2e' ([Guid]::NewGuid().ToString('N')))

if (-not (Test-Path -LiteralPath $Javac -PathType Leaf)) {
    Write-Error "e2e: javac not found at $Javac"
    exit 2
}
if (-not (Test-Path -LiteralPath $Notlin -PathType Leaf)) {
    Write-Error "e2e: notlin executable not found at $Notlin"
    exit 2
}
if (-not (Test-Path -LiteralPath $annotationsJar -PathType Leaf)) {
    Write-Error "e2e: annotations jar not found at $annotationsJar"
    exit 2
}

$samples = @(Get-ChildItem -LiteralPath $samplesDir -Filter '*.kt' -File | Sort-Object Name)
if ($samples.Count -eq 0) {
    Write-Error "e2e: no Kotlin samples found in $samplesDir"
    exit 2
}

New-Item -ItemType Directory -Path $outputRoot -Force | Out-Null
$failures = [System.Collections.Generic.List[string]]::new()
$passed = 0

foreach ($sample in $samples) {
    $caseDir = Join-Path $outputRoot $sample.BaseName
    $classesDir = Join-Path $caseDir 'classes'
    New-Item -ItemType Directory -Path $caseDir -Force | Out-Null
    Copy-Item -LiteralPath $sample.FullName -Destination (Join-Path $caseDir 'source.kt')

    $notlinLog = Join-Path $caseDir 'notlin.log'
    $transpileArgs = @('--allow-approximations', '--root', $caseDir, '-o', $caseDir, (Join-Path $caseDir 'source.kt'))
    & $Notlin @transpileArgs *> $notlinLog
    $notlinExit = $LASTEXITCODE
    if ($notlinExit -ne 0) {
        Write-Output "FAIL(notlin) $($sample.BaseName): exit $notlinExit (log: $notlinLog)"
        $failures.Add("$($sample.BaseName): notlin exit $notlinExit")
        continue
    }

    $javaSources = @(Get-ChildItem -LiteralPath $caseDir -Filter '*.java' -File | ForEach-Object { $_.FullName })
    if ($javaSources.Count -eq 0) {
        $javacLog = Join-Path $caseDir 'javac.log'
        Set-Content -LiteralPath $javacLog -Value 'transpiler produced no Java source files'
        Write-Output "FAIL(javac) $($sample.BaseName): no generated Java sources (log: $javacLog)"
        $failures.Add("$($sample.BaseName): no generated Java sources")
        continue
    }

    New-Item -ItemType Directory -Path $classesDir -Force | Out-Null
    $javacLog = Join-Path $caseDir 'javac.log'
    $javacArgs = @('-d', $classesDir, '-cp', $annotationsJar) + $javaSources
    & $Javac @javacArgs *> $javacLog
    $javacExit = $LASTEXITCODE
    if ($javacExit -ne 0) {
        Write-Output "FAIL(javac) $($sample.BaseName): exit $javacExit (log: $javacLog)"
        $failures.Add("$($sample.BaseName): javac exit $javacExit")
        continue
    }

    Write-Output "OK $($sample.BaseName)"
    $passed++
}

$failed = $failures.Count
Write-Output "e2e: $passed/$($samples.Count) passed; $failed failed"
Write-Output "logs: $outputRoot"
if ($failed -gt 0) {
    foreach ($failure in $failures) {
        Write-Output "  $failure"
    }
    exit 1
}
exit 0
