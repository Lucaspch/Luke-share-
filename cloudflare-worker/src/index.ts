/**
 * Luke-Share Edge Worker (Zero-Knowledge Room Codes + Ed25519 Verified Friends Inbox)
 *
 * Garantias de segurança:
 * 1. Salas Curtas (`PUT /v1/room/:blindHash` & `GET /v1/room/:blindHash`):
 *    Armazena apenas o ciphertext AES-256-GCM (Base58) sob um hash cego HKDF-SHA256 com expiração
 *    automática (`expirationTtl = 7200` = 2h). O Worker jamais vê o código nem o IP/nó real.
 * 2. Registro de Identidade (`POST /v1/tag/register` & `GET /v1/tag/:tag`):
 *    Verifica matematicamente `SHA-256(salt || pubkey) -> LK#XXXX` e valida assinatura Ed25519
 *    via WebCrypto (`crypto.subtle.verify("Ed25519", ...)`). Registro imutável (first-come-first-served).
 * 3. Caixa Postal Offline (`POST /v1/inbox` & `GET /v1/inbox/:tag?ts=...&sig=...`):
 *    Exige envelope assinado por Ed25519 para escrita e prova de posse da chave privada Ed25519 para leitura.
 */

export interface Env {
  LUKE_KV: KVNamespace;
}

const SAFE_ALPHABET = "23456789ABCDEFGHJKLMNPQRSTUVWXYZ";
const IDENTITY_SALT = new TextEncoder().encode("LUKE-SHARE-ED25519-IDENTITY-SALT-2026");
const B58_ALPHABET = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

function base58Decode(str: string): Uint8Array {
  const bytes: number[] = [0];
  for (let i = 0; i < str.length; i++) {
    const c = str[i];
    const val = B58_ALPHABET.indexOf(c);
    if (val < 0) throw new Error("Invalid Base58");
    let carry = val;
    for (let j = 0; j < bytes.length; j++) {
      carry += bytes[j] * 58;
      bytes[j] = carry & 0xff;
      carry >>= 8;
    }
    while (carry > 0) {
      bytes.push(carry & 0xff);
      carry >>= 8;
    }
  }
  for (let i = 0; i < str.length && str[i] === "1"; i++) {
    bytes.push(0);
  }
  return new Uint8Array(bytes.reverse());
}

function hexDecode(hex: string): Uint8Array {
  if (hex.length % 2 !== 0) throw new Error("Invalid hex");
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i++) {
    out[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  }
  return out;
}

async function deriveTag(pubkeyBytes: Uint8Array): Promise<string> {
  const buf = new Uint8Array(IDENTITY_SALT.length + pubkeyBytes.length);
  buf.set(IDENTITY_SALT, 0);
  buf.set(pubkeyBytes, IDENTITY_SALT.length);
  const hash = new Uint8Array(await crypto.subtle.digest("SHA-256", buf));
  let suffix = "";
  for (let i = 0; i < 4; i++) {
    suffix += SAFE_ALPHABET[hash[i] & 31];
  }
  return `LK#${suffix}`;
}

async function verifyEd25519(pubkeyBytes: Uint8Array, message: Uint8Array, sigBytes: Uint8Array): Promise<boolean> {
  if (pubkeyBytes.length !== 32 || sigBytes.length !== 64) return false;
  const key = await crypto.subtle.importKey("raw", pubkeyBytes, { name: "Ed25519" }, false, ["verify"]);
  return crypto.subtle.verify("Ed25519", key, sigBytes, message);
}

interface TagCard {
  tag: string;
  nick: string;
  pubkey_b58: string;
  signature_hex: string;
}

interface FriendEnvelope {
  from_tag: string;
  from_nick: string;
  from_pubkey_b58: string;
  to_tag: string;
  action: string;
  timestamp: number;
  signature_hex: string;
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    const path = url.pathname;

    try {
      // 1. Salas Curtas Criptografadas (AES-256-GCM Zero-Knowledge, TTL 2h)
      if (path.startsWith("/v1/room/")) {
        const hash = path.slice("/v1/room/".length).trim();
        if (!/^[0-9a-f]{32}$/.test(hash)) {
          return new Response("Invalid hash", { status: 400 });
        }
        const kvKey = `room:${hash}`;

        if (request.method === "PUT") {
          const body = (await request.text()).trim();
          if (!body || body.length > 8192) {
            return new Response("Invalid payload size", { status: 400 });
          }
          await env.LUKE_KV.put(kvKey, body, { expirationTtl: 7200 });
          return new Response("OK", { status: 200 });
        }

        if (request.method === "GET") {
          const val = await env.LUKE_KV.get(kvKey);
          if (!val) return new Response("Not Found", { status: 404 });
          return new Response(val, { status: 200 });
        }
      }

      // 2. Registro de Tag Ed25519
      if (path === "/v1/tag/register" && request.method === "POST") {
        const card = (await request.json()) as TagCard;
        const pubBytes = base58Decode(card.pubkey_b58);
        const expectedTag = await deriveTag(pubBytes);
        if (expectedTag !== card.tag) {
          return new Response("Tag mismatch", { status: 403 });
        }

        const msg = new TextEncoder().encode(`LUKE-TAG-CARD-V1:${card.tag}:${card.pubkey_b58}:${card.nick}`);
        const valid = await verifyEd25519(pubBytes, msg, hexDecode(card.signature_hex));
        if (!valid) {
          return new Response("Invalid signature", { status: 403 });
        }

        const kvKey = `tag:${card.tag}`;
        const existingRaw = await env.LUKE_KV.get(kvKey);
        if (existingRaw) {
          const existing = JSON.parse(existingRaw) as TagCard;
          if (existing.pubkey_b58 !== card.pubkey_b58) {
            return new Response("Tag already bound to another Ed25519 key", { status: 409 });
          }
        }

        await env.LUKE_KV.put(kvKey, JSON.stringify(card));
        return new Response("Registered", { status: 200 });
      }

      // 3. Consulta de TagCard
      if (path.startsWith("/v1/tag/") && request.method === "GET") {
        const tag = decodeURIComponent(path.slice("/v1/tag/".length)).toUpperCase();
        const val = await env.LUKE_KV.get(`tag:${tag}`);
        if (!val) return new Response("Not Found", { status: 404 });
        return new Response(val, {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
      }

      // 4. Envio de Envelope Assinado (Friend Request / Accept / Reject)
      if (path === "/v1/inbox" && request.method === "POST") {
        const envObj = (await request.json()) as FriendEnvelope;
        const pubBytes = base58Decode(envObj.from_pubkey_b58);
        if ((await deriveTag(pubBytes)) !== envObj.from_tag) {
          return new Response("Forged sender tag", { status: 403 });
        }

        const prefix = new TextEncoder().encode(
          `LUKE-ENV-V1:${envObj.from_tag}->${envObj.to_tag}:${envObj.action}:`
        );
        const tsBuf = new ArrayBuffer(8);
        new DataView(tsBuf).setBigUint64(0, BigInt(envObj.timestamp), false);
        const fullMsg = new Uint8Array(prefix.length + 8);
        fullMsg.set(prefix, 0);
        fullMsg.set(new Uint8Array(tsBuf), prefix.length);

        const valid = await verifyEd25519(pubBytes, fullMsg, hexDecode(envObj.signature_hex));
        if (!valid) {
          return new Response("Invalid envelope signature", { status: 403 });
        }

        const inboxKey = `inbox:${envObj.to_tag}`;
        const curRaw = await env.LUKE_KV.get(inboxKey);
        const list: FriendEnvelope[] = curRaw ? JSON.parse(curRaw) : [];
        const filtered = list.filter(
          (e) => !(e.from_tag === envObj.from_tag && e.action === envObj.action)
        );
        filtered.push(envObj);
        if (filtered.length > 50) filtered.shift();

        await env.LUKE_KV.put(inboxKey, JSON.stringify(filtered), { expirationTtl: 604800 });
        return new Response("Queued", { status: 200 });
      }

      // 5. Leitura Autenticada da Caixa Postal (exige assinatura da chave privada dona da tag)
      if (path.startsWith("/v1/inbox/") && request.method === "GET") {
        const tag = decodeURIComponent(path.slice("/v1/inbox/".length)).toUpperCase();
        const ts = url.searchParams.get("ts") || "";
        const sig = url.searchParams.get("sig") || "";

        const nowSec = Math.floor(Date.now() / 1000);
        const reqSec = parseInt(ts, 10);
        if (!reqSec || Math.abs(nowSec - reqSec) > 300) {
          return new Response("Expired timestamp", { status: 401 });
        }

        const tagRaw = await env.LUKE_KV.get(`tag:${tag}`);
        if (!tagRaw) return new Response("[]", { headers: { "Content-Type": "application/json" } });
        const card = JSON.parse(tagRaw) as TagCard;

        const authMsg = new TextEncoder().encode(`INBOX:${tag}:${ts}`);
        const ok = await verifyEd25519(base58Decode(card.pubkey_b58), authMsg, hexDecode(sig));
        if (!ok) {
          return new Response("Unauthorized", { status: 403 });
        }

        const inboxKey = `inbox:${tag}`;
        const inboxRaw = await env.LUKE_KV.get(inboxKey);
        if (!inboxRaw) {
          return new Response("[]", { headers: { "Content-Type": "application/json" } });
        }
        await env.LUKE_KV.delete(inboxKey);
        return new Response(inboxRaw, { headers: { "Content-Type": "application/json" } });
      }

      return new Response("Luke-Share Edge v0.5 Ready", { status: 200 });
    } catch (e: any) {
      return new Response(`Bad Request: ${e?.message || e}`, { status: 400 });
    }
  },
};
