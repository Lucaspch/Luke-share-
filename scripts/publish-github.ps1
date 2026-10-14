param(
    [string]$Token = $env:GITHUB_TOKEN,
    [string]$Tag = "v0.2.2",
    [string]$Repo = "Lucaspch/Luke-share-"
)

$ErrorActionPreference = "Stop"

$headers = @{
    "Authorization" = "token $Token"
    "Accept"        = "application/vnd.github.v3+json"
    "User-Agent"    = "Luke-Share-Release-Script"
}

Write-Host "Verificando ou criando Release $Tag no GitHub..." -ForegroundColor Cyan

# 1. Verifica se a release já existe
$release = $null
try {
    $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/tags/$Tag" -Method Get -Headers $headers
    Write-Host "Release encontrada (ID: $($release.id))" -ForegroundColor Green
} catch {
    Write-Host "Criando nova Release $Tag..." -ForegroundColor Yellow
    $body = @{
        tag_name   = $Tag
        name       = "Luke-Share $Tag - Ultra Bitrate & Qualidade de Imagem para Jogos"
        body       = "### O que há de novo na ${Tag}:`n`n- **Otimização de Bitrate para Jogos**: Aumento da taxa de bits padrão de 4.5 Mbps para até 20 Mbps (Ultra Gaming), eliminando artefatos de compressão e quadriculado em jogos rápidos (Forza Horizon, simuladores e shooters).`n- **Novo Seletor de Bitrate na Interface**: Escolha entre Auto (Recomendado), Ultra Gaming (20 Mbps), Alta (14 Mbps), Equilibrada (8 Mbps) ou Econômica (4 Mbps).`n- **Correção de Falso-Positivo de Perda de Pacotes**: Corrigido bug onde o handshake inicial registrava falsamente perda de rede e derrubava o bitrate para 1.5 Mbps.`n- **Recuperação Acelerada de Congestionamento (AIMD)**: Rampa de bitrate mais rápida para conexões estáveis.`n- **Ajustes de Alta Fidelidade no Media Foundation**: Margem de pico de 25% para cenas com muito movimento."
        draft      = $false
        prerelease = $false
    } | ConvertTo-Json -Depth 5

    $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases" -Method Post -Headers $headers -Body $body -ContentType "application/json"
    Write-Host "Release criada com sucesso! (ID: $($release.id))" -ForegroundColor Green
}

$releaseId = $release.id
$uploadBase = "https://uploads.github.com/repos/$Repo/releases/$releaseId/assets"

# 2. Arquivos para upload
$rootDir = Split-Path -Parent $PSScriptRoot
$distDir = Join-Path $rootDir "release-dist"

$filesToUpload = @(
    @{ Path = Join-Path $distDir "Luke-Share_0.2.2_x64-setup.exe"; Name = "Luke-Share_0.2.2_x64-setup.exe"; ContentType = "application/vnd.microsoft.portable-executable" },
    @{ Path = Join-Path $distDir "Luke-Share_0.2.2_x64-setup.exe.sig"; Name = "Luke-Share_0.2.2_x64-setup.exe.sig"; ContentType = "text/plain" },
    @{ Path = Join-Path $distDir "latest.json"; Name = "latest.json"; ContentType = "application/json" },
    @{ Path = Join-Path $rootDir "Luke-Share-v0.2.2-Windows.zip"; Name = "Luke-Share-v0.2.2-Windows.zip"; ContentType = "application/zip" }
)

# 3. Lista assets existentes para deletar duplicados se houver
$existingAssets = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/$releaseId/assets" -Method Get -Headers $headers

foreach ($item in $filesToUpload) {
    if (-not (Test-Path $item.Path)) {
        Write-Warning "Arquivo não encontrado: $($item.Path)"
        continue
    }

    $existing = $existingAssets | Where-Object { $_.name -eq $item.Name }
    if ($existing) {
        Write-Host "Removendo versão anterior de $($item.Name)..." -ForegroundColor Yellow
        Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/assets/$($existing.id)" -Method Delete -Headers $headers
    }

    $filePath = $item.Path
    $fileName = $item.Name
    $cType = $item.ContentType
    Write-Host "Enviando $fileName..." -ForegroundColor Cyan
    $uploadUri = "https://uploads.github.com/repos/$Repo/releases/$releaseId/assets?name=$fileName"
    
    & curl.exe -s -S -X POST -H "Authorization: token $Token" -H "Content-Type: $cType" --data-binary "@$filePath" "$uploadUri" | Out-Null
    Write-Host "✅ $fileName enviado com sucesso!" -ForegroundColor Green
}

Write-Host "`n🎉 Release $Tag publicada com sucesso no GitHub!" -ForegroundColor Green
Write-Host "Link da Release: $($release.html_url)" -ForegroundColor Cyan
