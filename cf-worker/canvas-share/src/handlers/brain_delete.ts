import type { Context } from 'hono';
import type { Env, BrainShareKVMeta } from '../types';
import { isValidShareId, verifyOwnerToken } from '../utils/validation';
import { jsonOk, jsonError, notFound, forbidden } from '../utils/responses';

interface RevokeBody {
  owner_token: string;
}

/**
 * DELETE /api/brain/share/:id — revoke. Requires the raw owner token.
 * 200 { deleted: true } | 400 | 403 | 404.
 */
export async function handleBrainDelete(c: Context<{ Bindings: Env }>): Promise<Response> {
  const shareId = c.req.param('id');
  if (!shareId || !isValidShareId(shareId)) {
    return jsonError('Invalid share ID format');
  }
  let body: RevokeBody;
  try {
    body = await c.req.json<RevokeBody>();
  } catch {
    return jsonError('Invalid JSON body');
  }
  if (!body.owner_token) {
    return jsonError('Missing required field: owner_token');
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
  await Promise.all([
    c.env.SHARE_BUCKET.delete(`brain_assets/${shareId}.bin`),
    c.env.SHARE_KV.delete(`brain:${shareId}`),
  ]);
  return jsonOk({ deleted: true });
}
