param(
    [string]$Version,
    [string]$Platform = "windows-x86_64"
)

$ErrorActionPreference = "Stop"

$repo = Resolve-Path (Join-Path $PSScriptRoot "..")
$dist = Join-Path $repo "target\dist"
$binaryPath = Join-Path $repo "target\release\mcmpu.exe"
if (-not (Test-Path -LiteralPath $binaryPath -PathType Leaf)) {
    throw "Built binary was not found at expected path: $binaryPath"
}

$reportedVersion = (& $binaryPath --version 2>&1 | Out-String).Trim()
if ($LASTEXITCODE -ne 0) {
    throw "Version probe failed with exit code $LASTEXITCODE for '$binaryPath': $reportedVersion"
}
if ($reportedVersion -notmatch '^mcmpu ((0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(\+[0-9a-f]{7})?)$') {
    throw "Version probe returned an invalid build version: $reportedVersion"
}
$binaryVersion = $Matches[1]
if (-not [string]::IsNullOrWhiteSpace($Version) -and $Version -cne $binaryVersion) {
    throw "Requested package version '$Version' does not match built binary version '$binaryVersion'."
}
$Version = $binaryVersion

$packageRoot = Join-Path $dist "mcmpu-$Version-$Platform"
$zipPath = Join-Path $dist "mcmpu-$Version-$Platform.zip"

New-Item -ItemType Directory -Force -Path $dist | Out-Null
if (Test-Path -LiteralPath $packageRoot) {
    Remove-Item -LiteralPath $packageRoot -Recurse -Force
}
if (Test-Path -LiteralPath $zipPath) {
    Remove-Item -LiteralPath $zipPath -Force
}

New-Item -ItemType Directory -Force -Path $packageRoot | Out-Null
Copy-Item -LiteralPath $binaryPath -Destination (Join-Path $packageRoot "mcmpu.exe") -Force
Set-Content -LiteralPath (Join-Path $packageRoot "mcmpu.bat") -Encoding ASCII -Value @(
    "@echo off",
    "setlocal",
    'set "MCMPU_DIR=%~dp0"',
    '"%MCMPU_DIR%mcmpu.exe" %*',
    "exit /b %ERRORLEVEL%"
)

Compress-Archive -Path (Join-Path $packageRoot "*") -DestinationPath $zipPath -CompressionLevel Optimal
Write-Output $zipPath
