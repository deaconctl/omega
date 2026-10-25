// Subscription gate: decides whether a phone's device may use the relay
// beyond pairing, from its RevenueCat `lathe_companion_pro` entitlement.
//
// KV keys:
//   active:<device_id>    cached active result, until min(expires, now + 1h)
//   last:<device_id>      last known active-until, kept 24h past it, for outages
//   desktops:<device_id>  desktops the device is paired with, for the webhook

// Phone ids are chosen by the app (one per install, shared across desktops),
// so they aren't derived from a key and only their shape is checked. The same
// id is the phone's RevenueCat app user id.
export const DEVICE_ID_PATTERN = /^[A-Za-z0-9_-]{8,64}$/;

export const ENTITLEMENT = "lathe_companion_pro";
const REVENUECAT_API = "https://api.revenuecat.com/v1/subscribers/";
const CACHE_MS = 60 * 60 * 1000;
export const OUTAGE_GRACE_MS = 24 * 60 * 60 * 1000;
// KV rejects expirations less than a minute away.
const MIN_KV_TTL_SECONDS = 60;
const MAX_DESKTOPS_PER_DEVICE = 50;

export interface Kv {
  get(key: string, type: "json"): Promise<unknown>;
  put(key: string, value: string, options?: { expirationTtl?: number }): Promise<void>;
  delete(key: string): Promise<void>;
}

export interface EntitlementEnv {
  SUBSCRIPTIONS?: Kv;
  REVENUECAT_SECRET_KEY?: string;
  REVENUECAT_WEBHOOK_AUTH?: string;
  // Any non-empty value lets every device in, for local development.
  REVENUECAT_DISABLED?: string;
}

// `null` means lifetime: RevenueCat reports no expiry for non-renewing access.
interface ActiveRecord {
  until: number | null;
}

type Lookup = { kind: "active"; until: number | null } | { kind: "inactive" } | { kind: "unavailable" };

export function gateDisabled(env: EntitlementEnv): boolean {
  return Boolean(env.REVENUECAT_DISABLED);
}

export class Entitlements {
  constructor(
    private env: EntitlementEnv,
    private now: () => number = Date.now,
    private fetcher: typeof fetch = (...args) => fetch(...args),
  ) {}

  // Active results are cached, so for a subscribed device RevenueCat is asked
  // at most once an hour. Inactive results never are, so a purchase made
  // after a 4402 is seen on the next attempt.
  async allows(deviceId: string): Promise<boolean> {
    if (gateDisabled(this.env)) return true;
    if (await this.cachedActive(deviceId)) return true;
    const lookup = await this.lookup(deviceId);
    if (lookup.kind === "active") return true;
    if (lookup.kind === "inactive") return false;
    return this.withinOutageGrace(deviceId);
  }

  async forget(deviceIds: string[], expired: boolean): Promise<void> {
    const kv = this.env.SUBSCRIPTIONS;
    if (!kv) return;
    await Promise.all(
      deviceIds.flatMap((deviceId) => [
        kv.delete(`active:${deviceId}`),
        // An expired subscription must not get back in through the outage rule.
        ...(expired ? [kv.delete(`last:${deviceId}`)] : []),
      ]),
    );
  }

  private async cachedActive(deviceId: string): Promise<boolean> {
    const record = (await this.env.SUBSCRIPTIONS?.get(`active:${deviceId}`, "json")) as ActiveRecord | null | undefined;
    // KV expiry is approximate, so the stored deadline is checked too.
    return record !== null && record !== undefined && (record.until === null || record.until > this.now());
  }

  private async withinOutageGrace(deviceId: string): Promise<boolean> {
    const record = (await this.env.SUBSCRIPTIONS?.get(`last:${deviceId}`, "json")) as ActiveRecord | null | undefined;
    if (!record) return false;
    return record.until === null || this.now() - record.until < OUTAGE_GRACE_MS;
  }

  private async lookup(deviceId: string): Promise<Lookup> {
    const secret = this.env.REVENUECAT_SECRET_KEY;
    if (!secret) {
      console.error("companion relay: REVENUECAT_SECRET_KEY is not set");
      return { kind: "unavailable" };
    }
    let response: Response;
    try {
      response = await this.fetcher(REVENUECAT_API + encodeURIComponent(deviceId), {
        headers: { Authorization: `Bearer ${secret}`, Accept: "application/json" },
      });
    } catch (error) {
      console.error("companion relay: RevenueCat is unreachable", error);
      return { kind: "unavailable" };
    }
    if (response.status === 404) return this.recordInactive(deviceId);
    if (!response.ok) {
      // A rejected key is a configuration problem rather than the device's
      // fault, so it gets the same grace as an outage.
      console.error(`companion relay: RevenueCat answered ${response.status}`);
      return { kind: "unavailable" };
    }

    let body: unknown;
    try {
      body = await response.json();
    } catch {
      console.error("companion relay: RevenueCat sent invalid JSON");
      return { kind: "unavailable" };
    }
    const until = activeUntil(body, this.now());
    if (until === undefined) return this.recordInactive(deviceId);
    await this.recordActive(deviceId, until);
    return { kind: "active", until };
  }

  private async recordActive(deviceId: string, until: number | null): Promise<void> {
    const kv = this.env.SUBSCRIPTIONS;
    if (!kv) return;
    const now = this.now();
    const cacheUntil = until === null ? now + CACHE_MS : Math.min(until, now + CACHE_MS);
    const cacheTtl = Math.floor((cacheUntil - now) / 1000);
    const writes: Promise<void>[] = [];
    // KV writes are the scarce resource (1,000 a day on the Free plan), and
    // the expiry only moves when the subscription renews.
    const last = (await kv.get(`last:${deviceId}`, "json")) as ActiveRecord | null;
    if (last?.until !== until) {
      writes.push(
        kv.put(`last:${deviceId}`, JSON.stringify({ until } satisfies ActiveRecord), {
          expirationTtl: until === null ? undefined : Math.max(MIN_KV_TTL_SECONDS, Math.floor((until + OUTAGE_GRACE_MS - now) / 1000)),
        }),
      );
    }
    // Too close to expiry to be worth caching; the next connect looks it up.
    if (cacheTtl >= MIN_KV_TTL_SECONDS) {
      writes.push(kv.put(`active:${deviceId}`, JSON.stringify({ until: cacheUntil } satisfies ActiveRecord), { expirationTtl: cacheTtl }));
    }
    await Promise.all(writes);
  }

  private async recordInactive(deviceId: string): Promise<Lookup> {
    // Deletes count as writes, so only clear an outage record that exists.
    if (await this.env.SUBSCRIPTIONS?.get(`last:${deviceId}`, "json")) await this.forget([deviceId], true);
    return { kind: "inactive" };
  }
}

// Returns the time the configured entitlement is active until, `null` for no
// expiry, or `undefined` when it isn't active.
export function activeUntil(body: unknown, now: number): number | null | undefined {
  const entitlement = (body as { subscriber?: { entitlements?: Record<string, unknown> } })?.subscriber?.entitlements?.[ENTITLEMENT] as
    | { expires_date?: string | null; grace_period_expires_date?: string | null }
    | undefined;
  if (!entitlement) return undefined;
  if (entitlement.expires_date === null || entitlement.expires_date === undefined) return null;
  // During a billing retry RevenueCat keeps access open until the grace
  // period ends, which can be later than `expires_date`.
  const candidates = [entitlement.expires_date, entitlement.grace_period_expires_date]
    .map((date) => (typeof date === "string" ? Date.parse(date) : NaN))
    .filter((time) => Number.isFinite(time));
  const latest = Math.max(...candidates);
  return Number.isFinite(latest) && latest > now ? latest : undefined;
}

// Which desktops each device is paired with, so the webhook can reach the
// Durable Objects holding its sockets. KV is eventually consistent, so a
// pairing seconds before an expiry can be missed; the device's next connect
// is still gated.
export class DesktopIndex {
  constructor(private kv: Kv | undefined) {}

  async desktopsFor(deviceId: string): Promise<string[]> {
    const list = await this.kv?.get(`desktops:${deviceId}`, "json");
    return Array.isArray(list) ? list.filter((id): id is string => typeof id === "string") : [];
  }

  async add(deviceId: string, desktopId: string): Promise<void> {
    if (!this.kv) return;
    const desktops = await this.desktopsFor(deviceId);
    if (desktops.includes(desktopId)) return;
    const next = [...desktops, desktopId].slice(-MAX_DESKTOPS_PER_DEVICE);
    await this.kv.put(`desktops:${deviceId}`, JSON.stringify(next));
  }

  async remove(deviceId: string, desktopId: string): Promise<void> {
    if (!this.kv) return;
    const desktops = await this.desktopsFor(deviceId);
    if (!desktops.includes(desktopId)) return;
    const next = desktops.filter((id) => id !== desktopId);
    if (next.length === 0) await this.kv.delete(`desktops:${deviceId}`);
    else await this.kv.put(`desktops:${deviceId}`, JSON.stringify(next));
  }
}

export interface WebhookDeps {
  env: EntitlementEnv;
  // Closes the device's sockets on one desktop with 4402.
  expireOnDesktop(desktopId: string, deviceId: string): Promise<void>;
  // Runs work after the response is sent.
  waitUntil(promise: Promise<unknown>): void;
}

// `POST /v1/revenuecat`. RevenueCat retries on anything but a 200, so the work
// that can be slow happens after responding.
export async function handleWebhook(request: Request, deps: WebhookDeps): Promise<Response> {
  if (request.method !== "POST") return new Response("method not allowed", { status: 405 });
  const expected = deps.env.REVENUECAT_WEBHOOK_AUTH;
  const presented = request.headers.get("Authorization") ?? "";
  if (!expected || !(await sameSecret(presented, expected))) {
    return new Response("unauthorized", { status: 401 });
  }

  let body: { event?: Record<string, unknown> };
  try {
    body = await request.json();
  } catch {
    return new Response("invalid json", { status: 400 });
  }
  const event = body.event ?? {};
  const deviceIds = affectedDeviceIds(event);
  const expired = event.type === "EXPIRATION";

  deps.waitUntil(
    (async () => {
      await new Entitlements(deps.env).forget(deviceIds, expired);
      if (!expired) return;
      const index = new DesktopIndex(deps.env.SUBSCRIPTIONS);
      await Promise.all(
        deviceIds.map(async (deviceId) => {
          const desktops = await index.desktopsFor(deviceId);
          await Promise.all(desktops.map((desktopId) => deps.expireOnDesktop(desktopId, deviceId)));
        }),
      );
    })().catch((error) => console.error("companion relay: webhook follow-up failed", error)),
  );
  return new Response("ok");
}

export function affectedDeviceIds(event: Record<string, unknown>): string[] {
  const ids = new Set<string>();
  const add = (value: unknown) => {
    if (typeof value === "string") ids.add(value);
    else if (Array.isArray(value)) value.forEach(add);
  };
  add(event.app_user_id);
  add(event.original_app_user_id);
  add(event.aliases);
  add(event.transferred_to);
  add(event.transferred_from);
  // RevenueCat's own anonymous ids ($RCAnonymousID:...) never reach the relay.
  return [...ids].filter((id) => DEVICE_ID_PATTERN.test(id));
}

// Compares digests so the time taken doesn't reveal how much of the secret
// matched.
async function sameSecret(presented: string, expected: string): Promise<boolean> {
  const encoder = new TextEncoder();
  const [left, right] = await Promise.all([
    crypto.subtle.digest("SHA-256", encoder.encode(presented)),
    crypto.subtle.digest("SHA-256", encoder.encode(expected)),
  ]);
  const a = new Uint8Array(left);
  const b = new Uint8Array(right);
  let difference = 0;
  for (let index = 0; index < a.length; index++) difference |= a[index] ^ b[index];
  return difference === 0;
}
