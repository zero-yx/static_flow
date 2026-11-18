$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$url = 'http://127.0.0.1:39080'
function Test-LocalSite {
    try { return (Invoke-RestMethod "$url/healthz" -TimeoutSec 2).service -eq 'static-flow/zero-yx' } catch { return $false }
}
if (Test-LocalSite) { Write-Output $url; return }
if (-not (Test-Path (Join-Path $repo '.local/site/index.html'))) { throw 'Build local frontend first.' }
$python = (Get-Command python.exe -ErrorAction Stop).Source
$app = Start-Process -FilePath $python -ArgumentList '-I','scripts/local_site.py' -WorkingDirectory $repo -WindowStyle Hidden -PassThru -RedirectStandardError (Join-Path $repo '.local/site-stderr.log') -RedirectStandardOutput (Join-Path $repo '.local/site-stdout.log')
for ($i=0;$i -lt 12;$i++) {
    Start-Sleep -Milliseconds 300
    if (Test-LocalSite) { Write-Output $url; return }
    if ($app.HasExited) { break }
}
throw 'Site failed to start. See .local/site-stderr.log and check port 39080.'
