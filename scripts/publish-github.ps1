param(
    [string]$Token = $env:GITHUB_TOKEN,
    [string]$Tag = "v0.3.0",
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
        name       = "Luke-Share $Tag - Mini-Player, Interface Discord Dark, Codecs AV1/HEVC & Prévia"
        body       = "### O que há de novo na ${Tag}:`n`n- **Picture-in-Picture (PiP) Nativo & Mini-Player**: Janela flutuante no canto da tela (Always on Top) para acompanhar a stream enquanto joga ou realiza outras tarefas.`n- **Interface Discord Dark**: Redesign visual completo com estética dark inspirada no Discord, contrastes aprimorados e listas de canais.`n- **Suporte a Codecs Modernos (AV1 e HEVC/H.265)**: Economia de 30% a 50% de bitrate com altíssima fidelidade para jogos rápidos, com fallback automático por hardware para H.264.`n- **Pré-visualização Antes de Iniciar**: Miniatura instantânea da fonte selecionada na interface antes de ligar a live."
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
