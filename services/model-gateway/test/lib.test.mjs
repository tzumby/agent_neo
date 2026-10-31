import assert from "node:assert/strict";
import { test } from "node:test";
import { callerDesktop, checkRequest, cost, day } from "../src/lib.mjs";

const config = { accountId: "461553546091", rolePrefix: "cloud-desktops-dev-desktop" };
const event = (userArn) => ({ requestContext: { authorizer: { iam: { accountId: "461553546091", userArn } } } });
const prices = { "zai-org/GLM-5.3": [1.4, 0.26, 4.4] };

test("only desktop roles' instance sessions are desktops", () => {
  const own = "arn:aws:sts::461553546091:assumed-role/cloud-desktops-dev-desktop-tzumby/i-0a1b2c3d4e5f60718";
  assert.equal(callerDesktop(event(own), config), "i-0a1b2c3d4e5f60718");
  assert.equal(callerDesktop(event(own.replace("/i-0a1b", "/me-0a1b")), config), null);
  assert.equal(callerDesktop(event("arn:aws:sts::461553546091:assumed-role/admin/i-0a1b2c3d4e5f60718"), config), null);
  assert.equal(callerDesktop(event("arn:aws:iam::461553546091:user/tzumby"), config), null);
});

test("requests: known models, bounded output, no streaming", () => {
  const messages = [{ role: "user", content: "hi" }];
  assert.deepEqual(checkRequest({ model: "zai-org/GLM-5.3", messages }, prices).body.max_tokens, 16384);
  assert.equal(checkRequest({ model: "zai-org/GLM-5.3", messages, max_tokens: 8192 }, prices).body.max_tokens, 8192);
  assert.match(checkRequest({ model: "openai/gpt-x", messages }, prices).error, /not allowed/);
  assert.match(checkRequest({ model: "zai-org/GLM-5.3", messages, stream: true }, prices).error, /streaming/);
  assert.match(checkRequest({ model: "zai-org/GLM-5.3", messages, max_tokens: 1e9 }, prices).error, /max_tokens/);
  assert.match(checkRequest({ model: "zai-org/GLM-5.3" }, prices).error, /messages/);
});

test("cost counts cached input at the cached price", () => {
  const usage = { prompt_tokens: 3000, prompt_tokens_details: { cached_tokens: 1000 }, completion_tokens: 500 };
  // 2000 × 1.4 + 1000 × 0.26 + 500 × 4.4, per million.
  assert.equal(cost(usage, prices["zai-org/GLM-5.3"]).toFixed(6), "0.005260");
  assert.equal(cost(undefined, prices["zai-org/GLM-5.3"]), 0);
  assert.equal(day(new Date("2026-10-31T23:59:59Z")), "2026-10-31");
});
