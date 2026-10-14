param(
    [string]$Version = "",
    [string]$Notes = "Melhorias de desempenho e correcoes de estabilidade.",
    [string]$Repo = "Lucaspch/Luke-share-"
)

$ErrorActionPreference = "Stop"

Write-Host "========================================" -ForegroundColor Cyan
Write-Host "    Luke-Share - Release & Auto-Updater Builder" -ForegroundColor Cyan
Write-Host "========================================" -ForegroundColor Cyan

$rootDir = Split-Path -Parent $PSScriptRoot
Set-Location $rootDir

# 1. Carrega ou atualiza versão
$tauriConfPath = Join-Path $rootDir "src-tauri\tauri.conf.json"
$cargoPath = Join-Path $rootDir "src-tauri\Cargo.toml"
$pkgPath = Join-Path $rootDir "package.json"
$htmlPath = Join-Path $rootDir "index.html"

$tauriConf = Get-Content $tauriConfPath -Raw | ConvertFrom-Json
$currentVer = $tauriConf.version

if ($Version -ne "" -and $Version -ne $currentVer) {
    Write-Host "Atualizando versao de $currentVer para $Version..." -ForegroundColor Yellow
    
    # Atualiza tauri.conf.json
    $tauriConf.version = $Version
    $tauriConf | ConvertTo-Json -Depth 10 | Set-Content $tauriConfPath -Encoding utf8
    
    # Atualiza package.json
    $pkg = Get-Content $pkgPath -Raw | ConvertFrom-Json
    $pkg.version = $Version
    $pkg | ConvertTo-Json -Depth 10 | Set-Content $pkgPath -Encoding utf8
    
    # Atualiza Cargo.toml
    $cargoContent = Get-Content $cargoPath -Raw
    $cargoContent = $cargoContent -replace 'version = "' + [regex]::Escape($currentVer) + '"', ('version = "' + $Version + '"')
    Set-Content $cargoPath $cargoContent -Encoding utf8
    
    # Atualiza index.html tag
    $htmlContent = Get-Content $htmlPath -Raw
    $htmlContent = $htmlContent -replace 'v' + [regex]::Escape($currentVer), ('v' + $Version)
    Set-Content $htmlPath $htmlContent -Encoding utf8
    
    $currentVer = $Version
}

Write-Host "Versao de Release: v$currentVer" -ForegroundColor Green

# 2. Carrega chave privada de assinatura
$keyPath = Join-Path $rootDir "src-tauri\.updater-key"
if (-not (Test-Path $keyPath)) {
    Write-Error "Chave privada de assinatura nao encontrada em: $keyPath. Execute 'npx tauri signer generate' primeiro."
    exit 1
}

$privateKey = Get-Content $keyPath -Raw
$env:TAURI_SIGNING_PRIVATE_KEY = $privateKey
$env:PATH = "C:\Program Files\CMake\bin;" + $env:PATH

# 3. Compilação Release com Tauri
Write-Host "Compilando binario e instalador assinado..." -ForegroundColor Yellow
npx tauri build

if ($LASTEXITCODE -ne 0) {
    Write-Error "Falha na compilacao do release."
    exit 1
}

# 4. Localiza os artefatos gerados
$nsisDir = Join-Path $rootDir "src-tauri\target\release\bundle\nsis"
$setupExe = Join-Path $nsisDir "Luke-Share_${currentVer}_x64-setup.exe"
$setupSig = Join-Path $nsisDir "Luke-Share_${currentVer}_x64-setup.exe.sig"
$standaloneExe = Join-Path $rootDir "src-tauri\target\release\luke-share.exe"

if (-not (Test-Path $setupExe)) {
    Write-Error "Instalador nao encontrado: $setupExe"
    exit 1
}

if (-not (Test-Path $setupSig)) {
    Write-Error "Arquivo de assinatura digital nao encontrado: $setupSig"
    exit 1
}

$signatureContent = (Get-Content $setupSig -Raw).Trim()

# 5. Prepara pasta release-dist
$distDir = Join-Path $rootDir "release-dist"
if (Test-Path $distDir) {
    Get-ChildItem -Path $distDir -File | Where-Object { $_.Name -like "*setup.exe*" -or $_.Name -like "*.sig" } | Remove-Item -Force -ErrorAction SilentlyContinue
} else {
    New-Item -ItemType Directory -Path $distDir | Out-Null
}

Copy-Item $standaloneExe (Join-Path $distDir "Luke-Share.exe") -Force
Copy-Item $setupExe (Join-Path $distDir "Luke-Share_${currentVer}_x64-setup.exe") -Force
Copy-Item $setupSig (Join-Path $distDir "Luke-Share_${currentVer}_x64-setup.exe.sig") -Force

if (Test-Path (Join-Path $rootDir "LEIA-ME.txt")) {
    Copy-Item (Join-Path $rootDir "LEIA-ME.txt") (Join-Path $distDir "LEIA-ME.txt") -Force
}

# 6. Gera o manifesto latest.json para o Tauri Updater
$pubDate = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
$downloadUrl = "https://github.com/$Repo/releases/download/v$currentVer/Luke-Share_${currentVer}_x64-setup.exe"

$latestManifest = @{
    version = $currentVer
    notes = $Notes
    pub_date = $pubDate
    platforms = @{
        "windows-x86_64" = @{
            signature = $signatureContent
            url = $downloadUrl
        }
    }
}

$latestJsonPath = Join-Path $distDir "latest.json"
$jsonStr = $latestManifest | ConvertTo-Json -Depth 5
$utf8NoBom = New-Object System.Text.UTF8Encoding $false
[System.IO.File]::WriteAllText($latestJsonPath, $jsonStr, $utf8NoBom)

# 7. Gera arquivo ZIP para distribuição direta
$zipName = "Luke-Share-v${currentVer}-Windows.zip"
$zipPath = Join-Path $rootDir $zipName
if (Test-Path $zipPath) {
    Remove-Item $zipPath -Force
}
Start-Sleep -Milliseconds 800
Compress-Archive -Path "$distDir\*" -DestinationPath $zipPath -Force

Write-Host ""
Write-Host "========================================" -ForegroundColor Green
Write-Host "   Release v$currentVer Gerado com Sucesso! " -ForegroundColor Green
Write-Host "========================================" -ForegroundColor Green
Write-Host "Artefatos prontos em: release-dist\" -ForegroundColor White
Write-Host "1. $distDir\Luke-Share.exe (Executavel Portatil)"
Write-Host "2. $distDir\Luke-Share_${currentVer}_x64-setup.exe (Instalador Oficial)"
Write-Host "3. $distDir\Luke-Share_${currentVer}_x64-setup.exe.sig (Assinatura Digital)"
Write-Host "4. $distDir\latest.json (Manifesto de Auto-Atualizacao)"
Write-Host "5. $zipPath (Pacote ZIP para Envio)"
Write-Host ""
Write-Host "Instrucoes para Publicar no GitHub Releases:" -ForegroundColor Yellow
Write-Host "1. Crie uma nova Release no GitHub com a tag: v$currentVer" -ForegroundColor White
Write-Host "2. Arraste e solte na Release os seguintes arquivos de 'release-dist\':" -ForegroundColor White
Write-Host "   - Luke-Share_${currentVer}_x64-setup.exe"
Write-Host "   - Luke-Share_${currentVer}_x64-setup.exe.sig"
Write-Host "   - latest.json"
Write-Host "   - $zipName"
Write-Host "3. Quando os amigos abrirem o Luke-Share, eles serao notificados automaticamente e poderao atualizar com 1 clique!" -ForegroundColor Cyan
