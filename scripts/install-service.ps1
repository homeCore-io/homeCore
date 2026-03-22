#Requires -Version 5.1
<#
.SYNOPSIS
    Install HomeCore as a Windows Service.

.DESCRIPTION
    Installs HomeCore as a Windows Service using either NSSM (Non-Sucking
    Service Manager, recommended) or the built-in sc.exe.

    Plugins are supervised by HomeCore itself, so only the homecore binary
    needs to be registered as a service.

    NSSM is strongly preferred: it handles stdout/stderr capture, restart
    policies, and working-directory configuration better than sc.exe.

    Download NSSM: https://nssm.cc/download
    Or via Chocolatey: choco install nssm
    Or via Scoop:      scoop install nssm
    Or via winget:     winget install nssm

.PARAMETER InstallDir
    HomeCore installation directory.
    Default: C:\HomeCore

.PARAMETER ServiceName
    Windows service name.
    Default: HomeCore

.PARAMETER DisplayName
    Service display name shown in Services MMC.
    Default: HomeCore Home Automation Server

.PARAMETER RunAs
    Account to run the service under.
    Default: LocalSystem
    Examples: "NT AUTHORITY\NetworkService", ".\homecore-svc", "DOMAIN\user"

.PARAMETER UseNssm
    Force use of NSSM even if sc.exe path is also available.
    Default: $true (NSSM is auto-detected; falls back to sc.exe if absent)

.PARAMETER Uninstall
    Remove the installed service.

.PARAMETER Status
    Show the current service status.

.EXAMPLE
    .\install-service.ps1
    Installs with all defaults (C:\HomeCore, LocalSystem account).

.EXAMPLE
    .\install-service.ps1 -InstallDir D:\homecore -RunAs "NT AUTHORITY\NetworkService"

.EXAMPLE
    .\install-service.ps1 -Uninstall
#>

[CmdletBinding(SupportsShouldProcess)]
param(
    [string]$InstallDir   = "C:\HomeCore",
    [string]$ServiceName  = "HomeCore",
    [string]$DisplayName  = "HomeCore Home Automation Server",
    [string]$Description  = "HomeCore home automation platform. Manages protocol plugins and provides REST/WebSocket API.",
    [string]$RunAs        = "LocalSystem",
    [switch]$UseNssm,
    [switch]$Uninstall,
    [switch]$Status
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
function Write-Step  { Write-Host "==> $args" -ForegroundColor Cyan }
function Write-Info  { Write-Host "    $args" }
function Write-Warn  { Write-Host "    WARN: $args" -ForegroundColor Yellow }
function Write-Fail  { Write-Host "ERROR: $args" -ForegroundColor Red; exit 1 }

function Require-Admin {
    $current = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($current)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        Write-Fail "This script must be run as Administrator. Re-launch PowerShell as Administrator."
    }
}

function Find-Nssm {
    # Check common install locations and PATH
    $candidates = @(
        (Get-Command nssm -ErrorAction SilentlyContinue)?.Source,
        "$env:ProgramFiles\NSSM\nssm.exe",
        "${env:ProgramFiles(x86)}\NSSM\nssm.exe",
        "$env:ChocolateyInstall\bin\nssm.exe",
        "$env:USERPROFILE\scoop\shims\nssm.exe"
    ) | Where-Object { $_ -and (Test-Path $_) }

    return $candidates | Select-Object -First 1
}

# ---------------------------------------------------------------------------
# Resolve paths
# ---------------------------------------------------------------------------
$BinaryPath = Join-Path $InstallDir "bin\homecore.exe"
$ConfigPath = Join-Path $InstallDir "config\homecore.toml"
$LogDir     = Join-Path $InstallDir "logs"

# ---------------------------------------------------------------------------
# Status
# ---------------------------------------------------------------------------
if ($Status) {
    $svc = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
    if ($svc) {
        Write-Step "Service: $ServiceName"
        Write-Info "Status  : $($svc.Status)"
        Write-Info "StartType: $($svc.StartType)"
        Write-Info "BinaryPath: $((Get-WmiObject Win32_Service -Filter "Name='$ServiceName'").PathName)"
    } else {
        Write-Host "Service '$ServiceName' is not installed."
    }
    exit 0
}

# ---------------------------------------------------------------------------
# Uninstall
# ---------------------------------------------------------------------------
if ($Uninstall) {
    Require-Admin

    $svc = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
    if (-not $svc) {
        Write-Warn "Service '$ServiceName' not found — nothing to remove."
        exit 0
    }

    Write-Step "Stopping service '$ServiceName'..."
    Stop-Service -Name $ServiceName -Force -ErrorAction SilentlyContinue

    $nssmPath = Find-Nssm
    if ($nssmPath) {
        Write-Step "Removing service with NSSM..."
        & $nssmPath remove $ServiceName confirm
    } else {
        Write-Step "Removing service with sc.exe..."
        sc.exe delete $ServiceName | Out-Null
    }

    Write-Step "Service '$ServiceName' removed."
    exit 0
}

# ---------------------------------------------------------------------------
# Install
# ---------------------------------------------------------------------------
Require-Admin

Write-Step "Installing HomeCore Windows Service"
Write-Info "Install dir  : $InstallDir"
Write-Info "Binary       : $BinaryPath"
Write-Info "Config       : $ConfigPath"
Write-Info "Service name : $ServiceName"
Write-Info "Run as       : $RunAs"

# Validate binary exists
if (-not (Test-Path $BinaryPath)) {
    Write-Fail "Binary not found: $BinaryPath`n       Run deploy.sh (WSL/Git Bash) or build first."
}

# Create log directory if absent
if (-not (Test-Path $LogDir)) {
    New-Item -ItemType Directory -Path $LogDir | Out-Null
    Write-Info "Created: $LogDir"
}

# Stop and remove existing service if present
$existing = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
if ($existing) {
    Write-Warn "Service '$ServiceName' already exists — replacing."
    Stop-Service -Name $ServiceName -Force -ErrorAction SilentlyContinue
    sc.exe delete $ServiceName | Out-Null
    Start-Sleep -Seconds 2
}

$nssmPath = Find-Nssm

# ---------------------------------------------------------------------------
# Install via NSSM (preferred)
# ---------------------------------------------------------------------------
if ($nssmPath -or $UseNssm) {
    if (-not $nssmPath) {
        Write-Fail "NSSM not found. Install with: choco install nssm  OR  scoop install nssm"
    }

    Write-Step "Installing service via NSSM: $nssmPath"

    & $nssmPath install         $ServiceName $BinaryPath
    & $nssmPath set             $ServiceName AppParameters "--home `"$InstallDir`" --config `"config\homecore.toml`""
    & $nssmPath set             $ServiceName AppDirectory  $InstallDir
    & $nssmPath set             $ServiceName DisplayName   $DisplayName
    & $nssmPath set             $ServiceName Description   $Description
    & $nssmPath set             $ServiceName Start         SERVICE_AUTO_START
    & $nssmPath set             $ServiceName ObjectName    $RunAs

    # Capture stdout/stderr to log files
    & $nssmPath set             $ServiceName AppStdout     (Join-Path $LogDir "homecore-stdout.log")
    & $nssmPath set             $ServiceName AppStderr     (Join-Path $LogDir "homecore-stderr.log")
    & $nssmPath set             $ServiceName AppRotateFiles 1
    & $nssmPath set             $ServiceName AppRotateOnline 1
    & $nssmPath set             $ServiceName AppRotateSeconds 86400
    & $nssmPath set             $ServiceName AppRotateBytes 10485760  # 10 MB

    # Restart policy: restart on failure, wait 5 s
    & $nssmPath set             $ServiceName AppExit Default Restart
    & $nssmPath set             $ServiceName AppRestartDelay 5000

    Write-Step "Service installed via NSSM."

# ---------------------------------------------------------------------------
# Install via sc.exe (fallback — limited restart/logging control)
# ---------------------------------------------------------------------------
} else {
    Write-Warn "NSSM not found — falling back to sc.exe (limited restart/logging support)."
    Write-Warn "Install NSSM for better reliability: choco install nssm"

    $binPathArg = "`"$BinaryPath`" --home `"$InstallDir`" --config `"config\homecore.toml`""

    sc.exe create $ServiceName `
        binPath= $binPathArg `
        DisplayName= $DisplayName `
        start= auto `
        obj= $RunAs | Out-Null

    sc.exe description $ServiceName $Description | Out-Null

    # Configure restart-on-failure (3 attempts, 5-second delays)
    sc.exe failure $ServiceName reset= 60 actions= restart/5000/restart/5000/restart/5000 | Out-Null

    Write-Step "Service installed via sc.exe."
}

Write-Host ""
Write-Step "Done. Next steps:"
Write-Info "Start service  : Start-Service $ServiceName"
Write-Info "                 (or: sc.exe start $ServiceName)"
Write-Info "Check status   : Get-Service $ServiceName"
Write-Info "                 (or: .\install-service.ps1 -Status)"
Write-Info "View logs      : Get-Content $LogDir\homecore-stderr.log -Tail 50 -Wait"
Write-Info "Stop service   : Stop-Service $ServiceName"
Write-Info "Uninstall      : .\install-service.ps1 -Uninstall"
Write-Host ""

# Offer to start immediately
$answer = Read-Host "Start the service now? [Y/n]"
if ($answer -match '^[Yy]?$') {
    Start-Service -Name $ServiceName
    Start-Sleep -Seconds 2
    $svc = Get-Service -Name $ServiceName
    Write-Step "Service status: $($svc.Status)"
}
