// Push relay: the only place the APNs key lives. Desktops call it through its
// IAM-authenticated function URL to register their owner's devices and to
// notify them. A desktop can only reach devices registered through itself.

import { createPrivateKey } from "node:crypto";
import { connect } from "node:http2";
import { DeleteItemCommand, DynamoDBClient, PutItemCommand, QueryCommand } from "@aws-sdk/client-dynamodb";
import { GetParameterCommand, SSMClient } from "@aws-sdk/client-ssm";
import { buildPayload, callerDesktop, outcome, signJwt, validDeviceToken } from "./lib.mjs";

const env = process.env;
const ddb = new DynamoDBClient({});
const ssm = new SSMClient({});
const MAX_DEVICES = 10;
// APNs wants a new token at most every 20 and at least every 60 minutes.
const TOKEN_LIFETIME_MS = 50 * 60 * 1000;

let keyPromise;
let cachedToken = { value: "", madeAt: 0 };

/** The .p8 key and its ID from SSM, read once per execution environment. */
function apnsKey() {
  keyPromise ??= Promise.all([
    ssm.send(new GetParameterCommand({ Name: env.KEY_PARAM, WithDecryption: true })),
    ssm.send(new GetParameterCommand({ Name: env.KEY_ID_PARAM })),
  ]).then(([key, id]) => ({ key: createPrivateKey(key.Parameter.Value), keyId: id.Parameter.Value }));
  keyPromise.catch(() => (keyPromise = undefined)); // retry on the next call
  return keyPromise;
}

async function providerToken() {
  if (Date.now() - cachedToken.madeAt < TOKEN_LIFETIME_MS) return cachedToken.value;
  const { key, keyId } = await apnsKey();
  cachedToken = { value: signJwt(key, keyId, env.TEAM_ID, Math.floor(Date.now() / 1000)), madeAt: Date.now() };
  return cachedToken.value;
}

const reply = (statusCode, body) => ({
  statusCode,
  headers: { "content-type": "application/json" },
  body: JSON.stringify(body),
});

function readBody(event) {
  try {
    const text = event.isBase64Encoded ? Buffer.from(event.body ?? "", "base64").toString() : (event.body ?? "");
    return text.length > 8192 ? null : JSON.parse(text || "{}");
  } catch {
    return null;
  }
}

async function devicesOf(desktop) {
  const result = await ddb.send(
    new QueryCommand({
      TableName: env.TABLE,
      KeyConditionExpression: "desktop = :d",
      ExpressionAttributeValues: { ":d": { S: desktop } },
      ProjectionExpression: "#t",
      ExpressionAttributeNames: { "#t": "token" },
    }),
  );
  return (result.Items ?? []).map((item) => item.token.S);
}

async function register(desktop, body) {
  const token = String(body.token ?? "").toLowerCase();
  if (!validDeviceToken(token)) return reply(400, { error: "token must be the APNs device token in hex" });
  const known = await devicesOf(desktop);
  if (!known.includes(token) && known.length >= MAX_DEVICES) return reply(429, { error: "too many devices" });
  await ddb.send(
    new PutItemCommand({
      TableName: env.TABLE,
      Item: {
        desktop: { S: desktop },
        token: { S: token },
        name: { S: String(body.name ?? "").slice(0, 80) },
        added: { S: new Date().toISOString() },
      },
    }),
  );
  console.log(JSON.stringify({ desktop, action: "register", devices: known.length + (known.includes(token) ? 0 : 1) }));
  return reply(200, { ok: true });
}

function sendOne(session, token, jwt, payload) {
  return new Promise((resolve) => {
    const request = session.request({
      ":method": "POST",
      ":path": `/3/device/${token}`,
      authorization: `bearer ${jwt}`,
      "apns-topic": env.TOPIC,
      "apns-push-type": "alert",
      "apns-priority": "10",
      "content-type": "application/json",
    });
    let status = 0;
    let body = "";
    request.setEncoding("utf8");
    request.setTimeout(10_000, () => request.close());
    request.on("response", (headers) => (status = headers[":status"]));
    request.on("data", (chunk) => (body += chunk));
    request.on("end", () => resolve(outcome(status, body)));
    request.on("error", (error) => resolve(`failed: ${error.code ?? error.message}`));
    request.end(JSON.stringify(payload));
  });
}

async function notify(desktop, body) {
  const payload = buildPayload(body);
  if (!payload) return reply(400, { error: "needs a title or body, and only known categories and data keys" });
  const tokens = await devicesOf(desktop);
  if (tokens.length === 0) return reply(200, { sent: 0, removed: 0 });
  const jwt = await providerToken();
  const session = connect(`https://${env.APNS_HOST}`);
  session.on("error", () => {});
  let sent = 0;
  let removed = 0;
  const failures = [];
  try {
    for (const token of tokens) {
      const result = await sendOne(session, token, jwt, payload);
      if (result === "delivered") sent += 1;
      else if (result === "gone") {
        removed += 1;
        await ddb.send(new DeleteItemCommand({ TableName: env.TABLE, Key: { desktop: { S: desktop }, token: { S: token } } }));
      } else failures.push(result);
    }
  } finally {
    session.close();
  }
  console.log(JSON.stringify({ desktop, action: "notify", sent, removed, failures }));
  return reply(failures.length && !sent ? 502 : 200, { sent, removed, failures });
}

export async function handler(event) {
  const desktop = callerDesktop(event, { accountId: env.ACCOUNT_ID, roleName: env.DESKTOP_ROLE_NAME });
  if (!desktop) {
    console.log(JSON.stringify({ action: "rejected", caller: event?.requestContext?.authorizer?.iam?.userArn ?? null }));
    return reply(403, { error: "only desktops can use the push relay" });
  }
  if (event.requestContext?.http?.method !== "POST") return reply(405, { error: "POST only" });
  const body = readBody(event);
  if (!body) return reply(400, { error: "body must be JSON (max 8 KB)" });
  switch (event.rawPath) {
    case "/devices":
      return register(desktop, body);
    case "/notify":
      return notify(desktop, body);
    default:
      return reply(404, { error: "unknown path" });
  }
}
