# ProxyGit Client Installer for Windows (WinFSP)
$ErrorActionPreference = "Stop"

Write-Host "==> ProxyGit Client v0.1.0 Installer for Windows" -ForegroundColor Green

# Check WinFSP
$winfspPath = "${env:ProgramFiles(x86)}\WinFSP"
if (-not (Test-Path $winfspPath)) {
    Write-Host "==> Installing WinFSP dependency via Winget..." -ForegroundColor Yellow
    winget install --id BillZissimopoulos.WinFsp -e --source winget
}

# Create config
$configDir = "$env:USERPROFILE\.config\proxygit"
New-Item -ItemType Directory -Force -Path $configDir | Out-Null

@"
server_addr = "127.0.0.1:8080"
drive_letter = "P:"
cache_dir = "$env:TEMP\proxygit\cache"
wal_dir = "$env:TEMP\proxygit\wal"
build_cache_dir = "$env:TEMP\proxygit\build_cache"
"@ | Out-File -FilePath "$configDir\config.toml" -Encoding utf8

Write-Host "==> ProxyGit Installed Successfully!" -ForegroundColor Green
Write-Host "==> Config created at $configDir\config.toml" -ForegroundColor Green
Write-Host "==> Start with: proxygit-client mount <server_addr> <project_id>" -ForegroundColor Green
