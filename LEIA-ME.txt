========================================================================
                 LUKE-SHARE - SCREENSHARE P2P NATIVO (v0.2.0)
========================================================================

Aplicativo de compartilhamento de tela de ultra-baixa latência P2P
(Peer-to-Peer nativo via Internet direta com Iroh QUIC/TLS 1.3).

SEM NECESSIDADE DE NENHUMA VPN (Zero Radmin, Zero Hamachi, 100% Gratuito).

RECURSOS PRINCIPAIS:
- P2P Direto sobre a Internet: Conexão via Código de Sala criptografado (lk-...).
- Taxa de Quadros Destravada: Detecção automática do framerate nativo do monitor
  (75Hz, 120Hz, 144Hz, 165Hz, 240Hz) ou modo Destravado de alta performance.
- Resoluções configuráveis: Origem (Nativa), 1080p, 720p ou 480p com aceleração
  por hardware NVENC / QuickSync / AMF.
- ISOLAMENTO ESTRITO DO DISCORD: O áudio da sua chamada no Discord é
  removido automaticamente da transmissão via WASAPI loopback, eliminando ecos!
- Remontagem de pacotes inteligente com NACKs e PLI sob demanda.

------------------------------------------------------------------------
COMO TRANSMITIR:
------------------------------------------------------------------------

1. Abra o Luke-Share e escolha o monitor ou janela de jogo que deseja compartilhar.
2. Selecione a Resolução e a Taxa de Quadros (FPS) desejada.
3. Clique em "▶ Iniciar Transmissão".
4. Copie o "Código da Sala" (lk-...) gerado no topo da tela e envie para seus amigos.

------------------------------------------------------------------------
COMO ASSISTIR:
------------------------------------------------------------------------

1. Abra o Luke-Share.
2. Cole o Código da Sala (lk-...) enviado pelo seu amigo no campo da barra lateral
   e clique em "Entrar".
3. A transmissão iniciará imediatamente a ultra-baixa latência!

------------------------------------------------------------------------
ATUALIZAÇÕES AUTOMÁTICAS (AUTO-UPDATER):
------------------------------------------------------------------------

O Luke-Share possui sistema de auto-atualização integrado com assinaturas
digitais seguras.
- Ao abrir o app, ele verifica silenciosamente se há uma versão mais recente.
- Você também pode clicar no botão "🔄 Atualizações" no cabeçalho do app.
- Se houver atualização, basta clicar em "⚡ Atualizar e Reiniciar" para que
  o app baixe e instale a nova versão automaticamente.

Para desenvolvedores / anfitrião lançar uma nova versão:
Execute no terminal:
   npm run release
ou passe a versão e notas:
   powershell -ExecutionPolicy Bypass -File scripts/build-release.ps1 -Version 0.2.1 -Notes "Novidades da versao"

------------------------------------------------------------------------
DESENVOLVIDO PARA MÁXIMO DESEMPENHO E BAIXA LATÊNCIA.
========================================================================
