param(
    [string]$Token = $env:GITHUB_TOKEN,
    [string]$Tag = "v0.3.1",
    [string]$Repo = "Lucaspch/Luke-share-",
    [string]$Title = "Luke-Share v0.3.1 - Correção 2K->1080p e Prévia ao Vivo",
    [string]$Description = "### O que há de novo na v0.3.1:`n`n- **Correção de Enquadramento em Monitores 2K (2560x1440)**: O Scaler D3D11 agora se adapta dinamicamente à resolução real capturada na GPU, garantindo que toda a tela seja reduzida para 1080p sem cortar barras laterais ou inferiores.`n- **Correção da Prévia ao Vivo e Carregamento Infinito**: O modo Auto agora prioriza o H.264 de ultra-baixa latência por hardware, ativando instantaneamente a prévia no host e nos espectadores sem ficar em loop infinito.`n- **Métricas Imediatas**: O player WebCodecs não exibe mais '0x0' ao sincronizar com o stream."
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
        name       = $Title
        body       = $Description
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
$cleanVer = $Tag.TrimStart('v')

$filesToUpload = @(
    @{ Path = Join-Path $distDir "Luke-Share_${cleanVer}_x64-setup.exe"; Name = "Luke-Share_${cleanVer}_x64-setup.exe"; ContentType = "application/vnd.microsoft.portable-executable" },
    @{ Path = Join-Path $distDir "Luke-Share_${cleanVer}_x64-setup.exe.sig"; Name = "Luke-Share_${cleanVer}_x64-setup.exe.sig"; ContentType = "text/plain" },
    @{ Path = Join-Path $distDir "latest.json"; Name = "latest.json"; ContentType = "application/json" },
    @{ Path = Join-Path $rootDir "Luke-Share-v${cleanVer}-Windows.zip"; Name = "Luke-Share-v${cleanVer}-Windows.zip"; ContentType = "application/zip" }
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
