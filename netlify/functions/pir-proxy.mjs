const ROUTES = new Map([
  ["/v1/status", "GET"],
  ["/v1/directory", "GET"],
  ["/v1/directory/tail", "GET"],
  ["/v1/query", "POST"],
]);

const REQUEST_HEADERS = ["accept", "content-type"];
const RESPONSE_HEADERS = ["content-type", "retry-after"];
const MAX_QUERY_BYTES = 4 * 1024 * 1024;
const UPSTREAM_TIMEOUT_MS = 25_000;

class ConfigurationError extends Error {}

function backendUrl(requestUrl) {
  const configured = process.env.PIR_BACKEND_URL?.trim();
  if (!configured) {
    throw new ConfigurationError("PIR_BACKEND_URL is not configured");
  }

  let backend;
  try {
    backend = new URL(configured);
  } catch {
    throw new ConfigurationError("PIR_BACKEND_URL is not a valid URL");
  }

  if (!["http:", "https:"].includes(backend.protocol)) {
    throw new ConfigurationError("PIR_BACKEND_URL must use http or https");
  }
  if (backend.username || backend.password || backend.search || backend.hash) {
    throw new ConfigurationError(
      "PIR_BACKEND_URL must not contain credentials, a query, or a fragment",
    );
  }

  const incoming = new URL(requestUrl);
  const prefix = backend.pathname === "/" ? "" : backend.pathname.replace(/\/+$/, "");
  backend.pathname = `${prefix}${incoming.pathname}`;
  backend.search = incoming.search;
  return backend;
}

function text(status, message, extraHeaders = {}) {
  return new Response(`${message}\n`, {
    status,
    headers: {
      "cache-control": "no-store",
      "content-type": "text/plain; charset=utf-8",
      ...extraHeaders,
    },
  });
}

export default async function pirProxy(request) {
  const incoming = new URL(request.url);
  const expectedMethod = ROUTES.get(incoming.pathname);
  if (!expectedMethod) return text(404, "not found");
  if (request.method !== expectedMethod) {
    return text(405, "method not allowed", { allow: expectedMethod });
  }

  let target;
  try {
    target = backendUrl(request.url);
  } catch (error) {
    if (error instanceof ConfigurationError) {
      console.error(`PIR proxy configuration: ${error.message}`);
      return text(500, error.message);
    }
    throw error;
  }

  const headers = new Headers();
  for (const name of REQUEST_HEADERS) {
    const value = request.headers.get(name);
    if (value) headers.set(name, value);
  }

  const init = {
    method: request.method,
    headers,
    redirect: "manual",
    signal: AbortSignal.timeout(UPSTREAM_TIMEOUT_MS),
  };
  if (request.method === "POST") {
    const contentType = request.headers.get("content-type")?.split(";", 1)[0].trim();
    if (contentType !== "application/octet-stream") {
      return text(415, "query content-type must be application/octet-stream");
    }

    const body = await request.arrayBuffer();
    if (body.byteLength === 0) return text(400, "empty query");
    if (body.byteLength > MAX_QUERY_BYTES) return text(413, "query is too large");
    init.body = body;
  }

  let upstream;
  try {
    upstream = await fetch(target, init);
  } catch (error) {
    if (init.signal.aborted) return text(504, "PIR backend timed out");
    console.error("PIR backend request failed", error);
    return text(502, "PIR backend unavailable");
  }

  const responseHeaders = new Headers({ "cache-control": "no-store" });
  for (const name of RESPONSE_HEADERS) {
    const value = upstream.headers.get(name);
    if (value) responseHeaders.set(name, value);
  }

  return new Response(await upstream.arrayBuffer(), {
    status: upstream.status,
    headers: responseHeaders,
  });
}

export const config = {
  path: "/v1/*",
};
