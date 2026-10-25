# Luke-Share Cloudflare Worker (Opcional)

O **Luke-Share v0.5.0** já opera 100% de forma nativa via **Iroh P2P Rendezvous (`dns.iroh.link`)** sem exigir servidor externo.

Caso você queira publicar também o cache global de borda na sua conta gratuita da Cloudflare (para caixa postal de pedidos de amizade enviados enquanto o destinatário está com o PC desligado e resolução instantânea `< 40ms` de códigos `LK-XXXX-XXXX`):

1. Autentique no Wrangler:
   ```powershell
   cd cloudflare-worker
   npx wrangler login
   ```
2. Crie o banco KV gratuito:
   ```powershell
   npx wrangler kv namespace create LUKE_KV
   ```
3. Copie o `id` exibido no terminal para `cloudflare-worker/wrangler.toml` e publique:
   ```powershell
   npx wrangler deploy
   ```
4. Salve a URL gerada (`https://luke-share-edge.<seu-subdominio>.workers.dev`) em `%APPDATA%\Luke-Share\worker_url.txt` (ou defina `LUKE_WORKER_URL`).
