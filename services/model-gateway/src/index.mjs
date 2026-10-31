// Model gateway: the only place the model provider's API key is used.
// Desktops call it through its IAM-authenticated function URL (SigV4 with
// their instance role) instead of holding the key; it allows known models,
// enforces a daily budget per desktop, and logs usage, never content.

import { DynamoDBClient, GetItemCommand, UpdateItemCommand } from "@aws-sdk/client-dynamodb";
import { GetParameterCommand, SSMClient } from "@aws-sdk/client-ssm";
import { callerDesktop, checkRequest, cost, day } from "./lib.mjs";

const env = process.env;
const ddb = new DynamoDBClient({});
const ssm = new SSMClient({});
const PRICES = JSON.parse(env.PRICES);
const BUDGET = Number(env.DAILY_BUDGET_USD);
const UPSTREAM = "https://api.together.ai/v1/chat/completions";

let keyPromise;
function apiKey() {
  keyPromise ??= ssm
    .send(new GetParameterCommand({ Name: env.KEY_PARAM, WithDecryption: true }))
    .then((result) => result.Parameter.Value);
  keyPromise.catch(() => (keyPromise = undefined)); // retry on the next call
  return keyPromise;
}

const reply = (statusCode, body, headers = {}) => ({
  statusCode,
  headers: { "content-type": "application/json", ...headers },
  body: typeof body === "string" ? body : JSON.stringify(body),
});
const failure = (statusCode, message) => reply(statusCode, { error: { message } });

async function spentToday(desktop, today) {
  const item = await ddb.send(
    new GetItemCommand({ TableName: env.TABLE, Key: { desktop: { S: desktop }, day: { S: today } } }),
  );
  return Number(item.Item?.cost?.N ?? 0);
}

async function record(desktop, today, usage, usd) {
  await ddb.send(
    new UpdateItemCommand({
      TableName: env.TABLE,
      Key: { desktop: { S: desktop }, day: { S: today } },
      UpdateExpression: "ADD cost :c, input_tokens :i, output_tokens :o, calls :one SET expires = :x",
      ExpressionAttributeValues: {
        ":c": { N: usd.toFixed(6) },
        ":i": { N: String(usage?.prompt_tokens ?? 0) },
        ":o": { N: String(usage?.completion_tokens ?? 0) },
        ":one": { N: "1" },
        ":x": { N: String(Math.floor(Date.now() / 1000) + 90 * 86400) },
      },
    }),
  );
}

export async function handler(event) {
  const desktop = callerDesktop(event, { accountId: env.ACCOUNT_ID, rolePrefix: env.DESKTOP_ROLE_PREFIX });
  if (!desktop) {
    console.log(JSON.stringify({ action: "rejected", caller: event?.requestContext?.authorizer?.iam?.userArn ?? null }));
    return failure(403, "only desktops can use the model gateway");
  }
  if (event.requestContext?.http?.method !== "POST" || event.rawPath !== "/v1/chat/completions") {
    return failure(404, "POST /v1/chat/completions only");
  }
  let request;
  try {
    request = JSON.parse(event.isBase64Encoded ? Buffer.from(event.body ?? "", "base64").toString() : event.body);
  } catch {
    return failure(400, "body must be JSON");
  }
  const checked = checkRequest(request, PRICES);
  if (checked.error) return failure(400, checked.error);

  const today = day();
  const spent = await spentToday(desktop, today);
  if (spent >= BUDGET) {
    console.log(JSON.stringify({ desktop, action: "over budget", spent }));
    return failure(429, `this desktop's daily model budget ($${BUDGET}) is used up`);
  }

  const started = Date.now();
  let upstream;
  try {
    upstream = await fetch(UPSTREAM, {
      method: "POST",
      headers: {
        authorization: `Bearer ${await apiKey()}`,
        "content-type": "application/json",
        // Cloudflare in front of the API rejects some default User-Agents (error 1010).
        "user-agent": "cloud-desktops-model-gateway/0.1",
      },
      body: JSON.stringify(checked.body),
      signal: AbortSignal.timeout(180_000),
    });
  } catch (error) {
    console.log(JSON.stringify({ desktop, action: "upstream unreachable", error: String(error?.name ?? error) }));
    return failure(502, "the model provider couldn't be reached");
  }
  const text = await upstream.text();
  let usd = 0;
  let usage;
  if (upstream.ok) {
    try {
      usage = JSON.parse(text).usage;
      usd = cost(usage, PRICES[checked.body.model]);
      await record(desktop, today, usage, usd);
    } catch (error) {
      console.log(JSON.stringify({ desktop, action: "usage not recorded", error: String(error) }));
    }
  }
  console.log(JSON.stringify({
    desktop, action: "chat", model: checked.body.model, status: upstream.status, ms: Date.now() - started,
    input: usage?.prompt_tokens ?? 0, output: usage?.completion_tokens ?? 0, cost: Number(usd.toFixed(6)),
  }));
  const retryAfter = upstream.headers.get("retry-after");
  return reply(upstream.status, text, retryAfter ? { "retry-after": retryAfter } : {});
}
