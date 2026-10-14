<#
  Teste automatizado do isolamento do Discord (não depende do Discord real):
   - tone_player.exe toca 440 Hz;
   - uma cópia renomeada ("discord_test.exe" / "Discord.exe") toca 880 Hz.
  O audio_probe usa o mesmo AudioCapturer do app e mede os tons com Goertzel.
  Esperado: 440 Hz presente, 880 Hz ausente (exclusão), em 4 cenários.
  Uso: powershell -File scripts\test-audio-exclusion.ps1   (toca tons de baixo volume por ~50 s)
#>
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$bin  = Join-Path $root 'src-tauri\target\debug'
$tmp  = Join-Path $bin 'audio-test'
New-Item -ItemType Directory -Force $tmp | Out-Null
$tone = Join-Path $bin 'tone_player.exe'
$probe = Join-Path $bin 'audio_probe.exe'
Copy-Item $tone (Join-Path $tmp 'discord_test.exe') -Force
Copy-Item $tone (Join-Path $tmp 'Discord.exe') -Force

$script:fail = 0
function Probe($probeArgs) {
  $out = & $probe @probeArgs
  $out | ForEach-Object { Write-Host "    $_" }
  $res = @{}
  foreach ($l in $out) { if ($l -match 'FREQ (\d+) Hz: (-?[\d\.]+) dB') { $res[[int]$matches[1]] = [double]$matches[2] } }
  return $res
}
function Check($name, $res, $present, $absent) {
  $ok = $true
  foreach ($f in $present) { if (-not $res.ContainsKey($f) -or $res[$f] -lt -40) { $ok = $false; Write-Host "    FALHA: $f Hz deveria estar presente ($($res[$f]) dB)" -ForegroundColor Red } }
  foreach ($f in $absent)  { if ($res.ContainsKey($f) -and $res[$f] -gt -60) { $ok = $false; Write-Host "    FALHA: $f Hz vazou ($($res[$f]) dB)" -ForegroundColor Red } }
  if ($ok) { Write-Host "[PASS] $name" -ForegroundColor Green } else { Write-Host "[FAIL] $name" -ForegroundColor Red; $script:fail++ }
}
function Start-Tone($exe, $freq, $secs) {
  Start-Process -FilePath $exe -ArgumentList "$freq $secs" -WindowStyle Hidden -PassThru
}

Write-Host "== A) Tela inteira, 1 raiz excluída (EXCLUDE_TREE): names=discord_test.exe"
$a = Start-Tone $tone 440 9; $b = Start-Tone (Join-Path $tmp 'discord_test.exe') 880 9
Start-Sleep -Milliseconds 1500
$r = Probe @('--mode','system','--seconds','5','--names','discord_test.exe')
Check 'A: sistema exclui a árvore do Discord (um processo)' $r @(440) @(880)
Wait-Process -Id $a.Id,$b.Id -ErrorAction SilentlyContinue

Write-Host "== B) Aplicativo específico (INCLUDE_TREE) = só o PID da janela/app"
$a = Start-Tone $tone 440 9; $b = Start-Tone (Join-Path $tmp 'discord_test.exe') 880 9
Start-Sleep -Milliseconds 1500
$r = Probe @('--mode','window','--pid',$a.Id,'--seconds','5','--names','discord_test.exe')
Check 'B: modo app captura somente o PID alvo' $r @(440) @(880)
Wait-Process -Id $a.Id,$b.Id -ErrorAction SilentlyContinue

Write-Host "== B2) Aplicativo específico apontando para o Discord => bloqueado"
$b = Start-Tone (Join-Path $tmp 'discord_test.exe') 880 7
Start-Sleep -Milliseconds 1500
$r = Probe @('--mode','window','--pid',$b.Id,'--seconds','4','--names','discord_test.exe')
Check 'B2: janela do Discord não gera áudio' $r @() @(880)
Wait-Process -Id $b.Id -ErrorAction SilentlyContinue

Write-Host "== C) Hot-swap: Discord abre DEPOIS do início da captura"
$a = Start-Tone $tone 440 12
$job = Start-Job { param($e) Start-Sleep -Milliseconds 2000; Start-Process -FilePath $e -ArgumentList '880 6' -WindowStyle Hidden } -ArgumentList (Join-Path $tmp 'discord_test.exe')
$r = Probe @('--mode','system','--seconds','8','--names','discord_test.exe','--skip','0.3')
Check 'C: Discord iniciado durante a transmissão continua excluído' $r @(440) @(880)
Wait-Process -Id $a.Id -ErrorAction SilentlyContinue; Remove-Job $job -Force
Get-Process discord_test -ErrorAction SilentlyContinue | Stop-Process -Force

Write-Host "== D) Padrão do app (Discord real rodando + Discord.exe de teste => multi-raiz, mixer INCLUDE)"
$a = Start-Tone $tone 440 9; $b = Start-Tone (Join-Path $tmp 'Discord.exe') 880 9
Start-Sleep -Milliseconds 1500
$r = Probe @('--mode','system','--seconds','5')
Check 'D: nomes padrão, Discord real + de teste' $r @(440) @(880)
Wait-Process -Id $a.Id,$b.Id -ErrorAction SilentlyContinue

if ($script:fail -eq 0) { Write-Host "`nTODOS OS TESTES DE ÁUDIO PASSARAM" -ForegroundColor Green; exit 0 }
Write-Host "`n$($script:fail) cenário(s) falharam" -ForegroundColor Red; exit 1
