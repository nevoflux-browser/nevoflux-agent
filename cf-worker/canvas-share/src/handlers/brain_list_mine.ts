import type { Context } from 'hono';
import type { Env } from '../types';
import { jsonOk } from '../utils/responses';

/**
 * GET /api/brain/list-mine
 *
 * Server-side per-sender enumeration is DEFERRED in v1 (no KV secondary
 * index). The sender's own list of created shares is maintained locally in
 * the daemon (`brain_shares` table). This endpoint exists so the contract
 * is documented and the route resolves; it always returns an empty list.
 */
export async function handleBrainListMine(_c: Context<{ Bindings: Env }>): Promise<Response> {
  return jsonOk({
    shares: [],
    note: 'server-side enumeration deferred; see daemon brain.share_list',
  });
}
