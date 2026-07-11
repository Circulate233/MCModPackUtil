use crate::server::files::ServerFilesManifest;
use crate::server::loader::LoaderInstallerPlan;

const JAVA_PATH_PLACEHOLDER: &str = "CHANGE_ME_JAVA_PATH";

pub fn render_start_bat(manifest: &ServerFilesManifest) -> String {
    let java_args = server_java_args(manifest);
    let mut script = format!(
        r#"@echo off
setlocal DisableDelayedExpansion
set "JAVA_EXE={java_path}"
set "CF_API_KEY={api_key}"
if "%JAVA_EXE%"=="{java_path}" (
  echo JAVA path is not configured. Edit start.bat and replace {java_path}.
  goto fail
)
"#,
        java_path = JAVA_PATH_PLACEHOLDER,
        api_key = escape_bat_value(&manifest.curseforge_api_key)
    );

    for file in &manifest.files {
        script.push_str(&format!(
            r#"
call :download_file "{path}" "{url}" "{sha1}" || goto fail
"#,
            path = file.path,
            url = file.url,
            sha1 = file.sha1
        ));
    }

    script.push_str(&render_loader_install_bat(&manifest.loader));
    script.push_str(
        &format!(
            r#"
set SERVER_JAR=server.jar
if not exist "%SERVER_JAR%" (
  for %%F in (fabric-server-launch.jar forge-*.jar neoforge-*.jar cleanroom-*.jar minecraft_server*.jar) do if exist "%%F" set SERVER_JAR=%%F
)
if not exist "%SERVER_JAR%" (
  echo Could not find installed server jar
  goto fail
)
"%JAVA_EXE%" {java_args} -jar "%SERVER_JAR%" nogui
if errorlevel 1 goto fail
exit /b 0

:download_file
if not exist "%~1" (
  powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0mcmpu-download.ps1" -Url "%~2" -OutFile "%~1" -Sha1 "%~3" -ApiKey "%CF_API_KEY%"
  if errorlevel 1 exit /b 1
)
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0mcmpu-download.ps1" -Url "%~2" -OutFile "%~1" -Sha1 "%~3" -ApiKey "%CF_API_KEY%" -VerifyOnly
if errorlevel 1 exit /b 1
exit /b 0

:fail
echo.
echo Server startup failed. Review the error above.
pause
exit /b 1
"#,
            java_args = java_args
        )
    );
    script
}

pub fn render_start_sh(manifest: &ServerFilesManifest) -> String {
    let java_args = server_java_args(manifest);
    let mut script = format!(
        r#"#!/usr/bin/env sh
set -eu
JAVA_EXE="{java_path}"
CF_API_KEY='{api_key}'
if [ "$JAVA_EXE" = "{java_path}" ]; then
  echo "JAVA path is not configured. Edit start.sh and replace {java_path}."
  exit 1
fi
"#,
        java_path = JAVA_PATH_PLACEHOLDER,
        api_key = shell_single_quoted_content(&manifest.curseforge_api_key)
    );

    for file in &manifest.files {
        script.push_str(&format!(
            r#"
if [ ! -f "{path}" ]; then
  mkdir -p "$(dirname "{path}")"
  if command -v curl >/dev/null 2>&1; then
    curl -L -H "x-api-key: $CF_API_KEY" "{url}" -o "{path}"
  else
    wget --header="x-api-key: $CF_API_KEY" "{url}" -O "{path}"
  fi
fi
if command -v sha1sum >/dev/null 2>&1; then
  echo "{sha1}  {path}" | sha1sum -c -
else
  ACTUAL="$(shasum -a 1 "{path}" | awk '{{print $1}}')"
  [ "$ACTUAL" = "{sha1}" ] || exit 1
fi
"#,
            path = file.path,
            url = file.url,
            sha1 = file.sha1
        ));
    }

    script.push_str(&render_loader_install_sh(&manifest.loader));
    script.push_str(
        &format!(
            r#"
SERVER_JAR="server.jar"
if [ ! -f "$SERVER_JAR" ]; then
  for candidate in fabric-server-launch.jar forge-*.jar neoforge-*.jar cleanroom-*.jar minecraft_server*.jar; do
    [ -f "$candidate" ] && SERVER_JAR="$candidate" && break
  done
fi
[ -f "$SERVER_JAR" ] || {{ echo "Could not find installed server jar"; exit 1; }}
"$JAVA_EXE" {java_args} -jar "$SERVER_JAR" nogui
"#,
            java_args = java_args
        )
    );
    script
}

pub fn render_download_ps1() -> &'static str {
    r#"param(
  [Parameter(Mandatory = $true)][string]$Url,
  [Parameter(Mandatory = $true)][string]$OutFile,
  [Parameter(Mandatory = $true)][string]$Sha1,
  [Parameter(Mandatory = $true)][string]$ApiKey,
  [switch]$VerifyOnly
)

$ErrorActionPreference = "Stop"

$parent = Split-Path -Parent $OutFile
if ($parent -and -not (Test-Path -LiteralPath $parent)) {
  New-Item -ItemType Directory -Force -Path $parent | Out-Null
}

if (-not $VerifyOnly) {
  Invoke-WebRequest -Uri $Url -Headers @{ "x-api-key" = $ApiKey } -OutFile $OutFile
}

if (-not (Test-Path -LiteralPath $OutFile)) {
  throw "Missing downloaded file: $OutFile"
}

$actual = (Get-FileHash -Algorithm SHA1 -LiteralPath $OutFile).Hash.ToLowerInvariant()
if ($actual -ne $Sha1.ToLowerInvariant()) {
  throw "SHA1 mismatch for $OutFile. Expected $Sha1 but got $actual"
}
"#
}

fn server_java_args(manifest: &ServerFilesManifest) -> String {
    let mut args = vec![
        format!("-Xms{}", manifest.java.min_memory),
        format!("-Xmx{}", manifest.java.max_memory),
    ];
    args.extend(manifest.java.extra_args.clone());
    args.join(" ")
}

fn render_loader_install_bat(loader: &LoaderInstallerPlan) -> String {
    match loader {
        LoaderInstallerPlan::Forge { installer_url, .. }
        | LoaderInstallerPlan::NeoForge { installer_url, .. }
        | LoaderInstallerPlan::Cleanroom { installer_url, .. } => format!(
            r#"
if not exist server.jar (
  powershell -NoProfile -ExecutionPolicy Bypass -Command "Invoke-WebRequest -Uri '{installer_url}' -OutFile 'loader-installer.jar'" || goto fail
  "%JAVA_EXE%" -jar loader-installer.jar --installServer || goto fail
)
"#
        ),
        LoaderInstallerPlan::Fabric {
            installer_url,
            minecraft_version,
            loader_version,
        } => format!(
            r#"
if not exist server.jar (
  powershell -NoProfile -ExecutionPolicy Bypass -Command "Invoke-WebRequest -Uri '{installer_url}' -OutFile 'fabric-installer.jar'" || goto fail
  "%JAVA_EXE%" -jar fabric-installer.jar server -mcversion {minecraft_version} -loader {loader_version} -downloadMinecraft || goto fail
)
"#
        ),
    }
}

fn escape_bat_value(value: &str) -> String {
    value.replace('%', "%%")
}

fn shell_single_quoted_content(value: &str) -> String {
    value.replace('\'', "'\"'\"'")
}

fn render_loader_install_sh(loader: &LoaderInstallerPlan) -> String {
    match loader {
        LoaderInstallerPlan::Forge { installer_url, .. }
        | LoaderInstallerPlan::NeoForge { installer_url, .. }
        | LoaderInstallerPlan::Cleanroom { installer_url, .. } => format!(
            r#"
if [ ! -f server.jar ]; then
  if command -v curl >/dev/null 2>&1; then
    curl -L "{installer_url}" -o loader-installer.jar
  else
    wget "{installer_url}" -O loader-installer.jar
  fi
  "$JAVA_EXE" -jar loader-installer.jar --installServer
fi
"#
        ),
        LoaderInstallerPlan::Fabric {
            installer_url,
            minecraft_version,
            loader_version,
        } => format!(
            r#"
if [ ! -f server.jar ]; then
  if command -v curl >/dev/null 2>&1; then
    curl -L "{installer_url}" -o fabric-installer.jar
  else
    wget "{installer_url}" -O fabric-installer.jar
  fi
  "$JAVA_EXE" -jar fabric-installer.jar server -mcversion {minecraft_version} -loader {loader_version} -downloadMinecraft
fi
"#
        ),
    }
}
