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
