param([switch]$Offline, [switch]$Stop)
$ErrorActionPreference = 'Stop'
Push-Location $PSScriptRoot
try {
    if ($Stop) {
        if (Test-NetConnection -ComputerName 127.0.0.1 -Port 8780 -InformationLevel Quiet -WarningAction SilentlyContinue) {
            Invoke-RestMethod -Method Post http://127.0.0.1:8780/api/shutdown | Out-Null
        } else {
            Write-Host 'No backend is listening on the demo port; stopping database fixtures.'
        }
        docker compose down
        if ($LASTEXITCODE -ne 0) { throw 'Docker Compose stop failed' }
        return
    }
    if (-not (Test-Path .env)) {
        Copy-Item .env.example .env
        Write-Host 'Created .env. Set FOUNDRY_PROJECT_ENDPOINT there for live agent calls.'
    }
    docker compose up -d --wait
    if ($LASTEXITCODE -ne 0) { throw 'Database startup failed' }
    Push-Location ui
    try {
        npm ci
        if ($LASTEXITCODE -ne 0) { throw 'Frontend dependency restore failed' }
        npm run build
        if ($LASTEXITCODE -ne 0) { throw 'Frontend build failed' }
    } finally { Pop-Location }
    $arguments = @('run', '--locked', '--', '--fixtures')
    if ($Offline) { $arguments += '--offline' }
    cargo @arguments
    if ($LASTEXITCODE -ne 0) { throw 'Demo process failed' }
} finally { Pop-Location }
