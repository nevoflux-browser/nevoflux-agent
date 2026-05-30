import { Hono } from 'hono';
import { cors } from 'hono/cors';
import type { Env } from './types';
import { handleUpload } from './handlers/upload';
import { handleFetchBundle } from './handlers/fetch';
import { handleMeta } from './handlers/meta';
import { handleExtend } from './handlers/extend';
import { handleDelete } from './handlers/delete';
import { handleLanding } from './handlers/landing';
import { handleBrainUpload } from './handlers/brain_upload';
import { handleBrainFetch } from './handlers/brain_fetch';
import { handleBrainRenew } from './handlers/brain_renew';
import { handleBrainDelete } from './handlers/brain_delete';
import { handleBrainListMine } from './handlers/brain_list_mine';

const app = new Hono<{ Bindings: Env }>();

// CORS middleware — allow NevoFlux extension requests
app.use(
  '/api/*',
  cors({
    origin: (origin) => {
      // Allow moz-extension:// origins and localhost for dev
      if (
        origin.startsWith('moz-extension://') ||
        origin.startsWith('chrome-extension://') ||
        origin.startsWith('http://localhost')
      ) {
        return origin;
      }
      return '';
    },
    allowMethods: ['GET', 'POST', 'PATCH', 'DELETE', 'OPTIONS'],
    allowHeaders: ['Content-Type', 'X-Owner-Token', 'X-Sender-Auth', 'X-Revoke-Token'],
    maxAge: 86400,
  })
);

// API routes
app.post('/api/share', handleUpload);
app.get('/api/share/:id/bundle', handleFetchBundle);
app.get('/api/share/:id/meta', handleMeta);
app.patch('/api/share/:id', handleExtend);
app.delete('/api/share/:id', handleDelete);

// Brain-share API routes (parallel `.nbrain` channel; brain_assets/ + brain: prefixes)
app.post('/api/brain/share', handleBrainUpload);
app.get('/api/brain/share/:id/bundle', handleBrainFetch);
app.patch('/api/brain/share/:id', handleBrainRenew);
app.delete('/api/brain/share/:id', handleBrainDelete);
app.get('/api/brain/list-mine', handleBrainListMine);

// Landing page
app.get('/c/:id', handleLanding);

// Health check
app.get('/health', (c) => c.json({ status: 'ok', service: 'nevoflux-canvas-share' }));

// Cron trigger handler (scheduled event for TTL cleanup)
export default {
  fetch: app.fetch,

  async scheduled(_event: ScheduledEvent, env: Env, ctx: ExecutionContext): Promise<void> {
    ctx.waitUntil(cleanupExpiredShares(env));
  },
};

/**
 * Remove expired shares from KV and R2.
 *
 * Sweeps both the canvas (`share:` / `share_assets`) and brain
 * (`brain:` / `brain_assets`) keyspaces; the two prefixes are disjoint.
 */
async function cleanupExpiredShares(env: Env): Promise<void> {
  await sweepPrefix(env, 'share:', 'share_assets');
  await sweepPrefix(env, 'brain:', 'brain_assets');
}

/**
 * List all KV keys with the given prefix and delete any (plus their R2
 * object under `r2Dir/`) whose `expires_at` timestamp is in the past.
 */
async function sweepPrefix(env: Env, kvPrefix: string, r2Dir: string): Promise<void> {
  const now = new Date().toISOString();
  let cursor: string | undefined;

  do {
    const list: KVNamespaceListResult<unknown, string> = await env.SHARE_KV.list({
      prefix: kvPrefix,
      cursor,
      limit: 100,
    });

    for (const key of list.keys) {
      const metaStr = await env.SHARE_KV.get(key.name);
      if (!metaStr) continue;

      try {
        const meta = JSON.parse(metaStr) as { expires_at: string };
        if (meta.expires_at < now) {
          const shareId = key.name.replace(kvPrefix, '');
          // Delete from R2 and KV in parallel
          await Promise.all([
            env.SHARE_BUCKET.delete(`${r2Dir}/${shareId}.bin`),
            env.SHARE_KV.delete(key.name),
          ]);
        }
      } catch {
        // Skip malformed entries
      }
    }

    cursor = list.list_complete ? undefined : list.cursor;
  } while (cursor);
}
