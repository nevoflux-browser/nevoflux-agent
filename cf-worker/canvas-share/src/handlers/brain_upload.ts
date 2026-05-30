import type { Context } from 'hono';
import type { Env, BrainShareKVMeta, BrainUploadResponse } from '../types';
import {
  isValidShareId,
  isValidOwnerTokenHash,
  hashIp,
  isValidNbrainMagic,
} from '../utils/validation';
import { jsonOk, jsonError, payloadTooLarge } from '../utils/responses';

/**
 * POST /api/brain/share — upload an opaque encrypted `.nbrain` blob.
 * Zero-knowledge: the worker never sees the content key.
 * Query: share_id, owner_token_hash, expiry_secs? ; Body: raw NBRN bytes.
 * Responses: 201 | 400 | 409 | 413.
 */
export async function handleBrainUpload(c: Context<{ Bindings: Env }>): Promise<Response> {
  const shareId = c.req.query('share_id');
  const ownerTokenHash = c.req.query('owner_token_hash');
  const expirySecs = parseInt(c.req.query('expiry_secs') || c.env.DEFAULT_EXPIRY_SECS, 10);

  if (!shareId || !ownerTokenHash) {
    return jsonError('Missing required query parameters: share_id, owner_token_hash');
  }
  if (!isValidShareId(shareId)) {
    return jsonError('Invalid share_id format (must be 10 chars Crockford base32)');
  }
  if (!isValidOwnerTokenHash(ownerTokenHash)) {
    return jsonError('Invalid owner_token_hash format (must be 64 hex chars)');
  }
  const maxLifetime = parseInt(c.env.MAX_LIFETIME_SECS, 10);
  if (expirySecs < 60 || expirySecs > maxLifetime) {
    return jsonError(`expiry_secs must be between 60 and ${maxLifetime}`);
  }

  const existing = await c.env.SHARE_KV.get(`brain:${shareId}`);
  if (existing) {
    return jsonError('Share ID already exists', 409);
  }

  const maxSize = parseInt(c.env.MAX_BRAIN_BUNDLE_SIZE, 10);
  const body = await c.req.arrayBuffer();
  if (body.byteLength === 0) {
    return jsonError('Empty request body');
  }
  if (body.byteLength > maxSize) {
    return payloadTooLarge(`Bundle size ${body.byteLength} exceeds maximum ${maxSize} bytes`);
  }
  if (!isValidNbrainMagic(body)) {
    return jsonError('Invalid bundle format (missing NBRN magic bytes)');
  }

  await c.env.SHARE_BUCKET.put(`brain_assets/${shareId}.bin`, body, {
    httpMetadata: { contentType: 'application/octet-stream' },
  });

  const now = new Date();
  const expiresAt = new Date(now.getTime() + expirySecs * 1000);
  const ip = c.req.header('cf-connecting-ip') || c.req.header('x-forwarded-for') || 'unknown';
  const ipHash = await hashIp(ip);

  const meta: BrainShareKVMeta = {
    created_at: now.toISOString(),
    expires_at: expiresAt.toISOString(),
    size_bytes: body.byteLength,
    owner_token_hash: ownerTokenHash,
    view_count: 0,
    ip_hash: ipHash,
  };
  await c.env.SHARE_KV.put(`brain:${shareId}`, JSON.stringify(meta), {
    expirationTtl: expirySecs + 86400,
  });

  const response: BrainUploadResponse = {
    share_id: shareId,
    expires_at: expiresAt.toISOString(),
    size_bytes: body.byteLength,
    url: `https://share.nevoflux.app/b/${shareId}`,
  };
  return jsonOk(response, 201);
}
