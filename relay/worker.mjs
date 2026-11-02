import { Relay } from "./route.mjs";
import { defaults } from "./limits.mjs";
export { Relay };
// One global coordinator, separate from the per-route objects. No isolate-local gate.
export class UpgradeGate {
  constructor(ctx, env) {
    this.ctx = ctx;
    this.env = env;
  }
  fetch() {
    return this.ctx.blockConcurrencyWhile(async () => {
      const second = Math.floor(Date.now() / 1000);
      const day = Math.floor(Date.now() / 86400000);
      let a = await this.ctx.storage.get("rate");
      if (a?.day !== day) a = { day, second, count: 0, daily: 0 };
      if (a.second !== second) {
        a.second = second;
        a.count = 0;
      }
      if (a.daily >= defaults.dailyUpgrades)
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
    let routes;
    try {
      routes = JSON.parse(env.ROUTES);
    } catch {
      return new Response(null, { status: 503 });
    }
    if (
      !Array.isArray(routes) ||
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
    const gate = env.UPGRADES.get(env.UPGRADES.idFromName("global"));
    const permit = await gate.fetch(request);
    if (permit.status !== 204) return new Response(null, { status: 429 });
    return env.ROUTES_DO.get(env.ROUTES_DO.idFromName(route)).fetch(request);
  },
};
