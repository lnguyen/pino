import path from "node:path";
import { pathToFileURL } from "node:url";

export const UPSTREAM_HOST = "api.anthropic.com";
export const BETA_FLAG = "extended-cache-ttl-2025-04-11";

// Client-sent breakpoints on system blocks smaller than this waste a slot.
// Caching <500 chars saves ~125 tokens but burns one of 4 breakpoints — not worth it.
export const MIN_SYSTEM_CACHE_CHARS = 500;

export const BREAKPOINT_CEILING = 4;

const VALID_TAIL_TTLS = new Set(["5m", "1h"]);

function parseTailTtl(raw) {
  if (!raw) return "5m";
  const v = String(raw).trim().toLowerCase();
  if (!VALID_TAIL_TTLS.has(v)) {
    console.error(`TAIL_TTL must be one of 5m|1h (got "${raw}"), falling back to 5m`);
    return "5m";
  }
  return v;
}

export function loadConfig() {
  const DASHBOARD = process.env.DASHBOARD === "1";
  return {
    PORT: Number(process.env.PORT) || 8898,
    BIND_HOST: process.env.BIND_HOST || "127.0.0.1",
    AUTO_CACHE: process.env.AUTO_CACHE === "1",
    LOG_BODIES: process.env.LOG_BODIES === "1",
    LOG_DIR: path.resolve(process.env.LOG_DIR || "./logs"),
    TRANSFORM_FILE: process.env.TRANSFORM_FILE || "",
    TAIL_TTL: parseTailTtl(process.env.TAIL_TTL),
    MODEL_OVERRIDE: process.env.MODEL_OVERRIDE || "",
    // Metering + dashboard. Dashboard implies metering (nothing to show otherwise).
    METRICS: process.env.METRICS === "1" || DASHBOARD,
    DASHBOARD,
    DASHBOARD_TOKEN: process.env.DASHBOARD_TOKEN || "",
    DB_PATH: path.resolve(process.env.DB_PATH || "./data/metrics.db"),
  };
}

export async function loadTransform(transformFile) {
  if (!transformFile) return null;
  const abs = path.resolve(transformFile);
  const mod = await import(pathToFileURL(abs).href);
  const fn = mod.transform || mod.default;
  if (typeof fn !== "function") {
    console.error(`TRANSFORM_FILE ${abs} must export transform(body) or default`);
    process.exit(1);
  }
  return fn;
}
