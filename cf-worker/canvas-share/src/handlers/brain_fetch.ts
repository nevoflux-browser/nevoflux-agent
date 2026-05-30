import type { Context } from 'hono';
import type { Env, BrainShareKVMeta } from '../types';
import { isValidShareId } from '../utils/validation';
import { jsonError, notFound } from '../utils/responses';

/**
 * GET /api/brain/share/:id/bundle — returns opaque NBRN ciphertext.
 * 200 | 400 | 404.
 */
export async function handleBrainFetch(c: Context<{ Bindings: Env }>): Promise<Response> {
  const shareId = c.req.param('id');
  if (!shareId || !isValidShareId(shareId)) {
    return jsonError('Invalid share ID format');
  }
  const metaStr = await c.env.SHARE_KV.get(`brain:${shareId}`);
  if (!metaStr) {
    return notFound('Share not found or expired');
  }
  const meta: BrainShareKVMeta = JSON.parse(metaStr);
  if (new Date(meta.expires_at) < new Date()) {
    return notFound('Share has expired');
  }
  const object = await c.env.SHARE_BUCKET.get(`brain_assets/${shareId}.bin`);
  if (!object) {
    return notFound('Share bundle not found in storage');
  }
  meta.view_count += 1;
  const remainingTtl = Math.max(
    60,
    Math.floor((new Date(meta.expires_at).getTime() - Date.now()) / 1000) + 86400
  );
  c.executionCtx.waitUntil(
    c.env.SHARE_KV.put(`brain:${shareId}`, JSON.stringify(meta), { expirationTtl: remainingTtl })
  );
  return new Response(object.body, {
    status: 200,
    headers: {
      'Content-Type': 'application/octet-stream',
      'Content-Length': meta.size_bytes.toString(),
      'Cache-Control': 'private, no-cache',
    },
  });
}
