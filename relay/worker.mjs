import { Relay } from "./route.mjs";
import { defaults, constantTimeToken } from "./limits.mjs";
export { Relay };
// The remote image page, served from the Worker's static assets (`web/`) on
// the relay's own origin. Only these exact paths are served; everything else
// falls through to the route handling below.
export const PAGE = new Map([
  ["/", "/remote/index.html"],
  ["/remote/app.mjs", "/remote/app.mjs"],
  ["/remote/client.mjs", "/remote/client.mjs"],
  ["/remote/keys.mjs", "/remote/keys.mjs"],
  ["/remote/model.mjs", "/remote/model.mjs"],
  ["/remote/tabs.mjs", "/remote/tabs.mjs"],
  ["/remote/view.mjs", "/remote/view.mjs"],
  ["/remote/remote.css", "/remote/remote.css"],
  ["/remote/icon.png", "/remote/icon.png"],
  ["/images/model.mjs", "/images/model.mjs"],
  ["/images/style.css", "/images/style.css"],
  ["/protocol/noise.mjs", "/protocol/noise.mjs"],
  ["/protocol/schema.mjs", "/protocol/schema.mjs"],
  ["/protocol/wire.mjs", "/protocol/wire.mjs"],
]);
const TYPES = {
  html: "text/html; charset=utf-8",
  mjs: "text/javascript; charset=utf-8",
  css: "text/css; charset=utf-8",
  png: "image/png",
};
// connect-src 'self' covers the same-origin wss: relay socket.
export const PAGE_HEADERS = Object.freeze({
  "Content-Security-Policy":
    "default-src 'none'; script-src 'self'; style-src 'self'; " +
    "img-src 'self' blob:; connect-src 'self'; base-uri 'none'; " +
    "form-action 'none'; frame-ancestors 'none'",
  "X-Content-Type-Options": "nosniff",
  "Referrer-Policy": "no-referrer",
  "Cross-Origin-Opener-Policy": "same-origin",
  "Cross-Origin-Resource-Policy": "same-origin",
  "Permissions-Policy": "camera=(), microphone=(), geolocation=(), payment=()",
  "X-Robots-Tag": "noindex",
  "Cache-Control": "no-cache",
});
async function page(request, env, path) {
  if (!["GET", "HEAD"].includes(request.method) || !env.ASSETS)
    return new Response(null, { status: 404 });
  const headers = new Headers();
  const tag = request.headers.get("If-None-Match");
  if (tag) headers.set("If-None-Match", tag);
  const asset = await env.ASSETS.fetch(
    new Request(new URL(path, request.url), {
      method: request.method,
      headers,
    }),
  );
  if (asset.status !== 200 && asset.status !== 304)
    return new Response(null, { status: 404 });
  const out = new Headers(PAGE_HEADERS);
  out.set("Content-Type", TYPES[path.split(".").pop()]);
  const etag = asset.headers.get("ETag");
  if (etag) out.set("ETag", etag);
  return new Response(asset.status === 304 ? null : asset.body, {
    status: asset.status,
    headers: out,
  });
}
// One global coordinator, separate from the per-route objects. No isolate-local gate.
export class UpgradeGate {
  constructor(ctx, env) {
    this.ctx = ctx;
    this.env = env;
  }
  fetch(request) {
    return this.ctx.blockConcurrencyWhile(async () => {
      const second = Math.floor(Date.now() / 1000);
      const day = Math.floor(Date.now() / 86400000);
      let a = await this.ctx.storage.get("rate");
      if (a?.day !== day) a = { day, second, count: 0, daily: 0 };
      if (a.second !== second) {
        a.second = second;
        a.count = 0;
      }
      if (
        a.daily >=
        (request?.headers.get("X-Homepool-Bridge") === "1"
          ? defaults.bridgeUpgrades
          : defaults.dailyUpgrades)
      )
        return new Response(null, { status: 429 });
      if (a.count >= defaults.upgradesPerSecond)
        return new Response(null, { status: 429 });
      a.count++;
      a.daily++;
      await this.ctx.storage.put("rate", a);
      return new Response(null, { status: 204 });
    });
  }
}
export default {
  async fetch(request, env) {
    if (env.KILL_SWITCH !== "false") return new Response(null, { status: 503 });
    const url = new URL(request.url);
    const asset = url.search ? undefined : PAGE.get(url.pathname);
    if (asset) return page(request, env, asset);
    let routes;
    try {
      routes = JSON.parse(env.ROUTES);
    } catch {
      return new Response(null, { status: 503 });
    }
    if (
      !Array.isArray(routes) ||
      routes.length > defaults.maxRoutes ||
      routes.some(
        (route) =>
          typeof route !== "string" || !/^[A-Za-z0-9_-]{1,64}$/.test(route),
      )
    )
      return new Response(null, { status: 503 });
    const route = url.pathname.slice(1);
    // Do not instantiate even the global gate for an unknown route.
    if (
      !Array.isArray(routes) ||
      !routes.includes(route) ||
      url.search ||
      request.method !== "GET"
    )
      return new Response(null, { status: 404 });
    if (request.headers.get("Upgrade")?.toLowerCase() !== "websocket")
      return new Response(null, { status: 426 });
    const incoming = new Headers(request.headers);
    incoming.delete("X-Homepool-Bridge");
    const authorization = incoming.get("Authorization");
    let bridge = false;
    if (authorization) {
      let secret;
      try {
        secret = JSON.parse(env.BRIDGE_TOKENS)[route];
      } catch {}
      const offered = authorization.replace(/^Bearer /, "");
      if (
        !authorization.startsWith("Bearer ") ||
        !/^[0-9a-f]{64}$/.test(secret ?? "") ||
        !/^[0-9a-f]{64}$/.test(offered) ||
        !(await constantTimeToken(
          new TextEncoder().encode(offered),
          new TextEncoder().encode(secret),
        ))
      )
        return new Response(null, { status: 403 });
      bridge = true;
      incoming.set("X-Homepool-Bridge", "1");
    }
    incoming.delete("Authorization");
    request = new Request(request, { headers: incoming });
    const gate = env.UPGRADES.get(
      env.UPGRADES.idFromName(bridge ? `bridge:${route}` : "global"),
    );
    const permit = await gate.fetch(request);
    if (permit.status !== 204) return new Response(null, { status: 429 });
    return env.ROUTES_DO.get(env.ROUTES_DO.idFromName(route)).fetch(request);
  },
};
