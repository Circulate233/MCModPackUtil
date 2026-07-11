param(
    [string]$Version = "0.1.0",
    [string]$Platform = "windows-x86_64"
)

$ErrorActionPreference = "Stop"

$repo = Resolve-Path (Join-Path $PSScriptRoot "..")
$dist = Join-Path $repo "target\dist"
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
Copy-Item -LiteralPath (Join-Path $repo "target\release\mcmpu.exe") -Destination (Join-Path $packageRoot "mcmpu.exe") -Force
Set-Content -LiteralPath (Join-Path $packageRoot "mcmpu.bat") -Encoding ASCII -Value @(
    "@echo off",
    "setlocal",
    'set "MCMPU_DIR=%~dp0"',
    '"%MCMPU_DIR%mcmpu.exe" %*',
    "exit /b %ERRORLEVEL%"
)

Compress-Archive -Path (Join-Path $packageRoot "*") -DestinationPath $zipPath -CompressionLevel Optimal
Write-Output $zipPath
