import { test, expect } from './coverage-helpers';
import { apiLogin } from './helpers';

/**
 * Error envelope — SPA-side companion to `tests/api/error_shape_normalized.hurl`.
 *
 * The normalize-errors pass (`docs/plan/normalize-errors.md`) rewrote
 * ~70 handler sites from ad-hoc `{"error": "..."}` tuples to the
 * uniform `ApiErrorBody` emitted by `AppError::IntoResponse`:
 *
 *     {status, error, message, error_type}
 *
 * Three places in the SPA read fields from an error response body:
 *   * `lib/api/endpoints/adminJobs.ts` (3 sites) — `body.error ?? body.message`
 *   * `lib/api/endpoints/opaque.ts::parseErrorBody` — `body.error_type`,
 *     `body.message ?? body.error`
 *   * `lib/api/transport.ts:230` — `body?.error_type === 'PasswordChangeRequired'`
 *
 * All three fall-through across both field names, so the normalization
 * is wire-compatible for every current consumer. These pins catch a
 * regression that would silently break the UX:
 *   * The server dropping a field the FE depended on.
 *   * The FE drifting to read a field the server no longer emits.
 *   * Phase 2 adding a `params` field in a way that breaks existing parsers.
 */

test.describe('error envelope', () => {
test.beforeEach(async ({ page }) => {
  await apiLogin(page);
});

test('admin job trigger — 404 on nonexistent name carries the normalized ApiErrorBody shape', async ({
  page,
}) => {
  // Hit the endpoint from the SPA's authenticated browser context so the
  // response passes through the fetch stack the FE actually uses (DPoP,
  // CSRF, interceptor) — a bare `page.request.*` from Node would skip
  // half of that. Reads the raw body so we're asserting the WIRE shape
  // the FE's `apiFetch` + `adminJobs.ts::triggerJob` would see.
  //
  // The CSRF double-submit cookie `oxicloud_csrf` is set at login; mirror
  // it into the `X-CSRF-Token` header so csrf_middleware doesn't 403
  // before we reach the handler. See frontend/src/lib/api/csrf.ts.
  const body = await page.evaluate(async () => {
    const match = document.cookie
      .split('; ')
      .find((row) => row.startsWith('oxicloud_csrf='));
    const csrfToken = match ? (match.split('=')[1] ?? '') : '';
    const res = await fetch('/api/admin/jobs/nonexistent-job-name/trigger', {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        'X-CSRF-Token': csrfToken
      },
      body: '{}',
      credentials: 'same-origin'
    });
    return { status: res.status, body: await res.json() };
  });
  expect(body.status).toBe(404);
  // Four-field shape. If any ONE of these is missing, the FE loses the
  // text it displays in the error toast.
  expect(body.body).toMatchObject({
    status: expect.stringContaining('404'),
    message: expect.any(String),
    error: expect.any(String),
    error_type: 'NotFound'
  });
  // The job name is folded into the message — pre-normalization it
  // lived in a sibling `name` field. Phase 2's `params` carrier will
  // move it back out; this pin flags that transition when it happens.
  expect(body.body.message).toContain('nonexistent-job-name');
  // Back-compat duplication: `.error` and `.message` must carry the
  // SAME string. Enforced by `AppError::IntoResponse` at
  // `interfaces/errors.rs:239`. Phase 2c retires `.error` after a
  // deprecation window — this assertion then goes along with it.
  expect(body.body.error).toBe(body.body.message);
});

test('favorites — 400 on bad item_type carries the normalized ApiErrorBody shape', async ({
  page,
}) => {
  // Previously `favorites_handler.rs:64` emitted a single-field
  // `{"error": "Item type must be ..."}` tuple. Pin that after the
  // normalization the shape is the full ApiErrorBody and the FE's
  // `body.error ?? body.message` fallbacks (used by every endpoint
  // module that renders a server error) still get a non-empty string.
  const body = await page.evaluate(async () => {
    const match = document.cookie
      .split('; ')
      .find((row) => row.startsWith('oxicloud_csrf='));
    const csrfToken = match ? (match.split('=')[1] ?? '') : '';
    const res = await fetch(
      '/api/favorites/not_a_valid_item_type/00000000-0000-0000-0000-000000000000',
      {
        method: 'POST',
        headers: { 'X-CSRF-Token': csrfToken },
        credentials: 'same-origin'
      }
    );
    return { status: res.status, body: await res.json() };
  });
  expect(body.status).toBe(400);
  expect(body.body).toMatchObject({
    status: expect.stringContaining('400'),
    message: expect.stringContaining('Item type must be'),
    error: expect.stringContaining('Item type must be'),
    error_type: 'BadRequest'
  });
});

test('OIDC backchannel-logout — 400 carries the OAuthErrorResponse shape, NOT ApiErrorBody', async ({
  page,
}) => {
  // The federated-IdP endpoint uses the RFC 6749 §5.2 shape
  // `{error, error_description}` — SPEC-REQUIRED, and some IdPs reject
  // responses carrying sibling fields they don't recognise. The
  // normalize-errors sweep must NOT have promoted this endpoint to
  // ApiErrorBody. Pin that the shape stays OAuth-flavoured and that
  // the four ApiErrorBody fields do NOT appear.
  const body = await page.evaluate(async () => {
    const params = new URLSearchParams({ logout_token: 'not.a.valid.jwt' });
    const res = await fetch('/api/auth/oidc/backchannel-logout', {
      method: 'POST',
      headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
      body: params.toString(),
      credentials: 'omit'
    });
    return { status: res.status, body: await res.json() };
  });
  // 400 (OIDC configured, bad token) or 503 (OIDC not configured) —
  // both carry the OAuth shape.
  expect([400, 503]).toContain(body.status);
  // RFC 6749 §5.2 shape.
  expect(body.body.error).toMatch(/^(invalid_request|authentication_not_configured)$/);
  expect(body.body.error_description).toEqual(expect.any(String));
  // ApiErrorBody fields must NOT appear here — a regression that
  // flipped the schema advertisement back to ApiErrorBody would make
  // the handler and the schema disagree, and some IdPs would reject
  // the response for the extra fields.
  expect(body.body.status).toBeUndefined();
  expect(body.body.message).toBeUndefined();
  expect(body.body.error_type).toBeUndefined();
});

});
