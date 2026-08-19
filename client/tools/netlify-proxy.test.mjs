import assert from "node:assert/strict";
import { after, beforeEach, test } from "node:test";

import pirProxy from "../../netlify/functions/pir-proxy.mjs";

const originalFetch = globalThis.fetch;
const originalBackend = process.env.PIR_BACKEND_URL;

let requests;

beforeEach(() => {
  requests = [];
  process.env.PIR_BACKEND_URL = "https://pir.example/prefix/";
  globalThis.fetch = async (target, init) => {
    const body = init.body ? new Uint8Array(init.body) : new Uint8Array();
    requests.push({ target: target.href, init, body });
    if (init.method === "POST") {
      return new Response(Uint8Array.from(body).reverse(), {
        status: 200,
        headers: { "content-type": "application/octet-stream" },
      });
    }
    return new Response('{"ok":true}', {
      status: 200,
      headers: { "content-type": "application/json", "retry-after": "3" },
    });
  };
});

after(() => {
  globalThis.fetch = originalFetch;
  if (originalBackend === undefined) delete process.env.PIR_BACKEND_URL;
  else process.env.PIR_BACKEND_URL = originalBackend;
});

test("proxies allowed GET routes through the configured path prefix", async () => {
  const response = await pirProxy(
    new Request("https://portal.example/v1/status?probe=1", {
      headers: { accept: "application/json", cookie: "do-not-forward=1" },
    }),
  );

  assert.equal(response.status, 200);
  assert.deepEqual(await response.json(), { ok: true });
  assert.equal(response.headers.get("cache-control"), "no-store");
  assert.equal(response.headers.get("retry-after"), "3");
  assert.equal(requests[0].target, "https://pir.example/prefix/v1/status?probe=1");
  assert.equal(requests[0].init.headers.get("accept"), "application/json");
  assert.equal(requests[0].init.headers.get("cookie"), null);
});

test("round-trips PIR query bytes without forwarding browser credentials", async () => {
  const query = Uint8Array.from([0, 1, 2, 253, 254, 255]);
  const response = await pirProxy(
    new Request("https://portal.example/v1/query", {
      method: "POST",
      headers: {
        authorization: "Bearer browser-secret",
        cookie: "do-not-forward=1",
        "content-type": "application/octet-stream",
      },
      body: query,
    }),
  );

  assert.equal(response.status, 200);
  assert.deepEqual(
    new Uint8Array(await response.arrayBuffer()),
    Uint8Array.from(query).reverse(),
  );
  assert.deepEqual(requests[0].body, query);
  assert.equal(requests[0].init.headers.get("content-type"), "application/octet-stream");
  assert.equal(requests[0].init.headers.get("authorization"), null);
  assert.equal(requests[0].init.headers.get("cookie"), null);
});

test("rejects unsupported routes, methods, and query bodies locally", async () => {
  let response = await pirProxy(new Request("https://portal.example/v1/unknown"));
  assert.equal(response.status, 404);

  response = await pirProxy(new Request("https://portal.example/v1/query"));
  assert.equal(response.status, 405);
  assert.equal(response.headers.get("allow"), "POST");

  response = await pirProxy(
    new Request("https://portal.example/v1/query", {
      method: "POST",
      headers: { "content-type": "text/plain" },
      body: "not binary",
    }),
  );
  assert.equal(response.status, 415);

  response = await pirProxy(
    new Request("https://portal.example/v1/query", {
      method: "POST",
      headers: { "content-type": "application/octet-stream" },
      body: new Uint8Array(),
    }),
  );
  assert.equal(response.status, 400);

  response = await pirProxy(
    new Request("https://portal.example/v1/query", {
      method: "POST",
      headers: { "content-type": "application/octet-stream" },
      body: new Uint8Array(4 * 1024 * 1024 + 1),
    }),
  );
  assert.equal(response.status, 413);
  assert.equal(requests.length, 0);
});
