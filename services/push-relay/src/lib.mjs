// Pure parts of the push relay (no AWS, no network), so they can be tested.

import { sign } from "node:crypto";

/** Notification categories the iOS app knows (Approve/Deny actions, etc.). */
export const CATEGORIES = new Set(["APPROVAL", "ACTION"]);

/** Keys a desktop may attach for the app; values are short ids. */
const DATA_KEYS = new Set(["task", "approval", "action"]);

/**
 * The desktop calling, from the function URL's IAM context: its EC2 instance
 * ID, when the caller is a desktop role's instance-profile session in this
 * account. Anything else gets null.
 *
 * Only safe because desktops may invoke the function through its URL alone
 * (lambda:InvokedViaFunctionUrl): a direct Invoke could forge this context.
 */
export function callerDesktop(event, { accountId, rolePrefix }) {
  const iam = event?.requestContext?.authorizer?.iam;
  if (!iam || iam.accountId !== accountId) return null;
  const match = /^arn:aws:sts::(\d{12}):assumed-role\/([\w+=,.@-]+)\/(i-[0-9a-f]{8,17})$/.exec(iam.userArn ?? "");
  if (!match || match[1] !== accountId) return null;
  // The fleet role, or a desktop's own role (<prefix>-<user>).
  const role = match[2];
  if (role !== rolePrefix && !role.startsWith(`${rolePrefix}-`)) return null;
  return match[3];
}

/** An APNs device token: hex, 32 bytes today; allow some growth. */
export function validDeviceToken(token) {
  return typeof token === "string" && /^[0-9a-f]{64,200}$/.test(token);
}

/** APNs provider token: ES256 JWT with the key ID and team ID. */
export function signJwt(privateKey, keyId, teamId, issuedAt) {
  const encode = (value) => Buffer.from(JSON.stringify(value)).toString("base64url");
  const input = `${encode({ alg: "ES256", kid: keyId })}.${encode({ iss: teamId, iat: issuedAt })}`;
  // JWT wants the raw r||s signature, not DER.
  const signature = sign("sha256", Buffer.from(input), { key: privateKey, dsaEncoding: "ieee-p1363" });
  return `${input}.${signature.toString("base64url")}`;
}

const clip = (text, max) => {
  const flat = String(text ?? "").replace(/\s+/g, " ").trim();
  return flat.length <= max ? flat : `${flat.slice(0, max - 1).trimEnd()}…`;
};

/**
 * Checks a desktop's notification and builds the APNs payload, or returns
 * null if it's malformed. Desktops are not trusted: only a title, a body, a
 * known category, a thread and a few short ids pass.
 */
export function buildPayload(input) {
  if (!input || typeof input !== "object") return null;
  const title = clip(input.title, 120);
  const body = clip(input.body, 400);
  if (!title && !body) return null;
  const aps = { alert: { title, body }, sound: "default" };
  if (input.category != null) {
    if (!CATEGORIES.has(input.category)) return null;
    aps.category = input.category;
  }
  if (input.thread != null) aps["thread-id"] = clip(input.thread, 64);
  const payload = { aps };
  for (const [key, value] of Object.entries(input.data ?? {})) {
    if (DATA_KEYS.has(key) && typeof value === "string" && /^[\w-]{1,64}$/.test(value)) payload[key] = value;
  }
  return payload;
}

/** What to do with a device after APNs answered. */
export function outcome(status, body) {
  if (status === 200) return "delivered";
  let reason = "";
  try {
    reason = JSON.parse(body).reason ?? "";
  } catch {}
  if (status === 410 || reason === "BadDeviceToken" || reason === "Unregistered") return "gone";
  return `failed: ${status} ${reason}`.trim();
}
