import { describe, it, expect } from 'vitest';
import { SELF } from 'cloudflare:test';
import { isValidNbrainMagic } from '../src/utils/validation';

describe('isValidNbrainMagic', () => {
  it('accepts a buffer beginning with NBRN', () => {
    const buf = new Uint8Array([0x4e, 0x42, 0x52, 0x4e, 0x01, 0xaa]).buffer;
    expect(isValidNbrainMagic(buf)).toBe(true);
  });
  it('rejects the canvas NFEB magic', () => {
    const buf = new Uint8Array([0x4e, 0x46, 0x45, 0x42]).buffer;
    expect(isValidNbrainMagic(buf)).toBe(false);
  });
  it('rejects a buffer shorter than 4 bytes', () => {
    const buf = new Uint8Array([0x4e, 0x42, 0x52]).buffer;
    expect(isValidNbrainMagic(buf)).toBe(false);
  });
  it('rejects an empty buffer', () => {
    expect(isValidNbrainMagic(new ArrayBuffer(0))).toBe(false);
  });
});

const NBRN = () => {
  const bytes = new Uint8Array(64);
  bytes.set([0x4e, 0x42, 0x52, 0x4e]); // NBRN
  return bytes;
};
const OWNER_HASH = 'a'.repeat(64);

describe('POST /api/brain/share', () => {
  it('stores an NBRN blob and returns share_id + url', async () => {
    const res = await SELF.fetch(
      `https://share.nevoflux.app/api/brain/share?share_id=abc123xyz0&owner_token_hash=${OWNER_HASH}`,
      { method: 'POST', body: NBRN() }
    );
    expect(res.status).toBe(201);
    const body = await res.json<{ share_id: string; url: string; size_bytes: number }>();
    expect(body.share_id).toBe('abc123xyz0');
    expect(body.url).toContain('/b/abc123xyz0');
    expect(body.size_bytes).toBe(64);
  });

  it('rejects a non-NBRN body with 400', async () => {
    const res = await SELF.fetch(
      `https://share.nevoflux.app/api/brain/share?share_id=def456ghj0&owner_token_hash=${OWNER_HASH}`,
      { method: 'POST', body: new Uint8Array([0x4e, 0x46, 0x45, 0x42]) }
    );
    expect(res.status).toBe(400);
  });

  it('round-trips the blob via GET /api/brain/share/:id/bundle', async () => {
    await SELF.fetch(
      `https://share.nevoflux.app/api/brain/share?share_id=jkm789npq0&owner_token_hash=${OWNER_HASH}`,
      { method: 'POST', body: NBRN() }
    );
    const res = await SELF.fetch('https://share.nevoflux.app/api/brain/share/jkm789npq0/bundle');
    expect(res.status).toBe(200);
    const back = new Uint8Array(await res.arrayBuffer());
    expect(back[0]).toBe(0x4e);
    expect(back.length).toBe(64);
  });

  it('returns 404 for an unknown bundle', async () => {
    const res = await SELF.fetch('https://share.nevoflux.app/api/brain/share/zzzzzzzzzz/bundle');
    expect(res.status).toBe(404);
  });
});

async function uploadWithToken(shareId: string) {
  const token = new Uint8Array(32).fill(7);
  const enc = new TextEncoder();
  const combined = new Uint8Array(shareId.length + 32);
  combined.set(enc.encode(shareId));
  combined.set(token, shareId.length);
  const digest = new Uint8Array(await crypto.subtle.digest('SHA-256', combined));
  const hash = [...digest].map((b) => b.toString(16).padStart(2, '0')).join('');
  const tokenB64 = btoa(String.fromCharCode(...token))
    .replace(/\+/g, '-')
    .replace(/\//g, '_')
    .replace(/=+$/, '');
  const blob = new Uint8Array(64);
  blob.set([0x4e, 0x42, 0x52, 0x4e]);
  await SELF.fetch(
    `https://share.nevoflux.app/api/brain/share?share_id=${shareId}&owner_token_hash=${hash}`,
    { method: 'POST', body: blob }
  );
  return tokenB64;
}

describe('PATCH /api/brain/share/:id (renew)', () => {
  it('extends expiry with a valid owner token', async () => {
    const tok = await uploadWithToken('rnw1111110');
    const res = await SELF.fetch('https://share.nevoflux.app/api/brain/share/rnw1111110', {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ owner_token: tok, extend_secs: 86400 }),
    });
    expect(res.status).toBe(200);
    const body = await res.json<{ share_id: string; expires_at: string }>();
    expect(body.share_id).toBe('rnw1111110');
  });

  it('rejects a wrong owner token with 403', async () => {
    await uploadWithToken('rnw2222220');
    const wrong = btoa(String.fromCharCode(...new Uint8Array(32).fill(9)))
      .replace(/\+/g, '-')
      .replace(/\//g, '_')
      .replace(/=+$/, '');
    const res = await SELF.fetch('https://share.nevoflux.app/api/brain/share/rnw2222220', {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ owner_token: wrong, extend_secs: 86400 }),
    });
    expect(res.status).toBe(403);
  });
});

describe('DELETE /api/brain/share/:id (revoke)', () => {
  it('deletes with a valid owner token then 404s the bundle', async () => {
    // Note: share IDs must be valid Crockford base32 (no i/l/o/u).
    const tok = await uploadWithToken('den1111110');
    const del = await SELF.fetch('https://share.nevoflux.app/api/brain/share/den1111110', {
      method: 'DELETE',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ owner_token: tok }),
    });
    expect(del.status).toBe(200);
    const fetched = await SELF.fetch(
      'https://share.nevoflux.app/api/brain/share/den1111110/bundle'
    );
    expect(fetched.status).toBe(404);
  });
});

describe('GET /api/brain/list-mine', () => {
  it('returns an empty shares array (server-side enumeration deferred)', async () => {
    const res = await SELF.fetch('https://share.nevoflux.app/api/brain/list-mine', {
      headers: { 'X-Sender-Auth': 'anything' },
    });
    expect(res.status).toBe(200);
    const body = await res.json<{ shares: unknown[] }>();
    expect(Array.isArray(body.shares)).toBe(true);
  });
});
