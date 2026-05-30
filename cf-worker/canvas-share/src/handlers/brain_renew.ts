import type { Context } from 'hono';
import type { Env, BrainShareKVMeta } from '../types';
import { isValidShareId, verifyOwnerToken } from '../utils/validation';
import { jsonOk, jsonError, notFound, forbidden } from '../utils/responses';

interface RenewBody {
  owner_token: string;
  extend_secs: number;
}

/**
 * PATCH /api/brain/share/:id — extend TTL. Requires the raw owner token.
 * Caps total lifetime at MAX_LIFETIME_SECS (365 days). 200 | 400 | 403 | 404.
 * Note: takes seconds (extend_secs) for symmetry with canvas; the spec's
 * `extend_days` is converted to seconds by the caller (UI/skill).
 */
export async function handleBrainRenew(c: Context<{ Bindings: Env }>): Promise<Response> {
  const shareId = c.req.param('id');
  if (!shareId || !isValidShareId(shareId)) {
    return jsonError('Invalid share ID format');
  }
  let body: RenewBody;
  try {
    body = await c.req.json<RenewBody>();
  } catch {
    return jsonError('Invalid JSON body');
  }
  if (!body.owner_token || !body.extend_secs) {
    return jsonError('Missing required fields: owner_token, extend_secs');
  }
  if (body.extend_secs < 60) {
    return jsonError('extend_secs must be at least 60');
  }
  const metaStr = await c.env.SHARE_KV.get(`brain:${shareId}`);
  if (!metaStr) {
    return notFound('Share not found or expired');
  }
  const meta: BrainShareKVMeta = JSON.parse(metaStr);
  const isOwner = await verifyOwnerToken(shareId, body.owner_token, meta.owner_token_hash);
  if (!isOwner) {
    return forbidden('Invalid owner token');
  }
  const maxLifetime = parseInt(c.env.MAX_LIFETIME_SECS, 10);
  const createdAt = new Date(meta.created_at).getTime();
  const currentExpiry = new Date(meta.expires_at).getTime();
  const newExpiry = currentExpiry + body.extend_secs * 1000;
  if ((newExpiry - createdAt) / 1000 > maxLifetime) {
    return jsonError(`Extension would exceed maximum lifetime of ${maxLifetime} seconds`);
  }
  const newExpiresAt = new Date(newExpiry);
  meta.expires_at = newExpiresAt.toISOString();
  const remainingTtl = Math.max(60, Math.floor((newExpiry - Date.now()) / 1000) + 86400);
  await c.env.SHARE_KV.put(`brain:${shareId}`, JSON.stringify(meta), {
    expirationTtl: remainingTtl,
  });
  return jsonOk({ share_id: shareId, expires_at: newExpiresAt.toISOString() });
}
