$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$url = 'http://127.0.0.1:39082/api/healthz'
function Test-Axum {
    try { $h = Invoke-RestMethod $url -TimeoutSec 2; return ($h.status -eq 'ok' -and $h.port -eq 39082) } catch { return $false }
}
if (Test-Axum) { Write-Output $url; return }
$child = Start-Process wsl.exe -ArgumentList '-d','Ubuntu-20.04','--','bash','scripts/start-local-axum.sh' -WorkingDirectory $root -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $root '.local/axum-start.log') -RedirectStandardError (Join-Path $root '.local/axum-errors.log')
for ($i=0; $i -lt 30; $i++) {
    Start-Sleep -Milliseconds 800
    if (Test-Axum) { Write-Output $url; return }
    if ($child.HasExited) { break }
}
throw 'Axum not reachable; see .local/axum-errors.log and WSL log/boot.log.'
