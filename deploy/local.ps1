# Runs the gateway in the foreground of a terminal on the owner's own PC.
# The bot token is read from .env here and never printed. Ctrl-C stops the gateway.
# Start it from a terminal the owner keeps open: a process started by an agent
# session dies with that session, without a line in the log.
$repo = Split-Path $PSScriptRoot -Parent
$home_dir = 'C:/Users/Wonde/agentsflow-home'
$logs = Join-Path $home_dir '.agentsflow'
Set-Location $repo
foreach ($line in Get-Content (Join-Path $repo '.env')) {
    $text = $line.Trim()
    if ($text -eq '' -or $text.StartsWith('#')) { continue }
    $text = $text -replace '^export\s+', ''
    $at = $text.IndexOf('=')
    if ($at -lt 1) { continue }
    $value = $text.Substring($at + 1).Trim().Trim('"').Trim("'")
    [Environment]::SetEnvironmentVariable($text.Substring(0, $at).Trim(), $value, 'Process')
}
Write-Host "agentsflow: running on port 7878. Log: $logs\server.log. Ctrl-C stops it."
Start-Process -FilePath (Join-Path $repo 'target\debug\agentsflow.exe') `
    -ArgumentList 'serve', '--home', $home_dir, '--telegram', '--port', '7878' `
    -NoNewWindow -Wait `
    -RedirectStandardOutput (Join-Path $logs 'gateway.out') `
    -RedirectStandardError (Join-Path $logs 'gateway.err')
Write-Host 'agentsflow: stopped.'
