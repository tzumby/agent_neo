import assert from "node:assert/strict";
import { generateKeyPairSync, verify } from "node:crypto";
import { test } from "node:test";
import { buildPayload, callerDesktop, outcome, signJwt, validDeviceToken } from "../src/lib.mjs";

const config = { accountId: "461553546091", roleName: "cloud-desktops-dev-desktop" };
const event = (userArn, accountId = "461553546091") => ({
  requestContext: { authorizer: { iam: { accountId, userArn } } },
});

test("only the desktop role's instance sessions in this account are desktops", () => {
  const desktop = "arn:aws:sts::461553546091:assumed-role/cloud-desktops-dev-desktop/i-0f2c0538dc0b1c670";
  assert.equal(callerDesktop(event(desktop), config), "i-0f2c0538dc0b1c670");
  // Another role, a user, a non-instance session, another account, no IAM context.
  assert.equal(callerDesktop(event("arn:aws:sts::461553546091:assumed-role/admin/i-0f2c0538dc0b1c670"), config), null);
  assert.equal(callerDesktop(event("arn:aws:iam::461553546091:user/tzumby"), config), null);
  assert.equal(callerDesktop(event("arn:aws:sts::461553546091:assumed-role/cloud-desktops-dev-desktop/me"), config), null);
  assert.equal(callerDesktop(event(desktop.replace("461553546091", "111122223333"), "111122223333"), config), null);
  assert.equal(callerDesktop({ requestContext: { authorizer: null } }, config), null);
});

test("signs a verifiable ES256 provider token", () => {
  const { privateKey, publicKey } = generateKeyPairSync("ec", { namedCurve: "P-256" });
  const token = signJwt(privateKey, "MMUYJ4GC7Z", "4UVAPZYV4Q", 1_700_000_000);
  const [header, claims, signature] = token.split(".");
  assert.deepEqual(JSON.parse(Buffer.from(header, "base64url")), { alg: "ES256", kid: "MMUYJ4GC7Z" });
  assert.deepEqual(JSON.parse(Buffer.from(claims, "base64url")), { iss: "4UVAPZYV4Q", iat: 1_700_000_000 });
  const ok = verify("sha256", Buffer.from(`${header}.${claims}`), { key: publicKey, dsaEncoding: "ieee-p1363" },
    Buffer.from(signature, "base64url"));
  assert.ok(ok);
});

test("payloads keep only known fields", () => {
  const payload = buildPayload({
    title: "Run this command?",
    body: "sudo apt update\n(it runs as root)",
    category: "APPROVAL",
    thread: "t1",
    data: { approval: "a1b2c3", task: "t1", url: "https://evil.example", action: "<script>" },
  });
  assert.deepEqual(payload, {
    aps: {
      alert: { title: "Run this command?", body: "sudo apt update (it runs as root)" },
      sound: "default",
      category: "APPROVAL",
      "thread-id": "t1",
    },
    approval: "a1b2c3",
    task: "t1",
  });
  assert.equal(buildPayload({ title: "x", category: "OPEN_URL" }), null);
  assert.equal(buildPayload({}), null);
  assert.equal(buildPayload({ body: "x".repeat(1000) }).aps.alert.body.length, 400);
});

test("device tokens and APNs answers", () => {
  assert.ok(validDeviceToken("a".repeat(64)));
  assert.ok(!validDeviceToken("A".repeat(64)));
  assert.ok(!validDeviceToken("abc"));
  assert.equal(outcome(200, ""), "delivered");
  assert.equal(outcome(410, '{"reason":"Unregistered"}'), "gone");
  assert.equal(outcome(400, '{"reason":"BadDeviceToken"}'), "gone");
  assert.equal(outcome(403, '{"reason":"InvalidProviderToken"}'), "failed: 403 InvalidProviderToken");
});
