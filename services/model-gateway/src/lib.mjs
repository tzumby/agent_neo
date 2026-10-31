// Pure parts of the model gateway (no AWS, no network), so they can be tested.

/**
 * The desktop calling, from the function URL's IAM context: its EC2 instance
 * ID, when the caller is a desktop role's instance-profile session in this
 * account (the fleet role, or a desktop's own role <prefix>-<user>).
 *
 * Only safe because desktops may invoke the function through its URL alone
 * (lambda:InvokedViaFunctionUrl): a direct Invoke could forge this context.
 */
export function callerDesktop(event, { accountId, rolePrefix }) {
  const iam = event?.requestContext?.authorizer?.iam;
  if (!iam || iam.accountId !== accountId) return null;
  const match = /^arn:aws:sts::(\d{12}):assumed-role\/([\w+=,.@-]+)\/(i-[0-9a-f]{8,17})$/.exec(iam.userArn ?? "");
  if (!match || match[1] !== accountId) return null;
  const role = match[2];
  if (role !== rolePrefix && !role.startsWith(`${rolePrefix}-`)) return null;
  return match[3];
}

/** Output tokens a request may ask for. */
export const MAX_OUTPUT_TOKENS = 16384;

/**
 * Checks a chat-completion request from a desktop: a known model, a sane
 * output limit, no streaming (the gateway answers in one piece). Returns
 * { body } to forward, or { error }.
 */
export function checkRequest(request, prices) {
  if (!request || typeof request !== "object" || !Array.isArray(request.messages)) {
    return { error: "expected a chat completion request with messages" };
  }
  if (!Object.hasOwn(prices, request.model)) return { error: `model not allowed: ${request.model}` };
  if (request.stream) return { error: "streaming isn't supported through the gateway" };
  const maxTokens = Number(request.max_tokens ?? MAX_OUTPUT_TOKENS);
  if (!Number.isFinite(maxTokens) || maxTokens < 1 || maxTokens > MAX_OUTPUT_TOKENS) {
    return { error: `max_tokens must be 1..${MAX_OUTPUT_TOKENS}` };
  }
  return { body: { ...request, max_tokens: maxTokens } };
}

/** USD for one response's usage, from [input, cached input, output] per 1M tokens. */
export function cost(usage, [input, cached, output]) {
  const prompt = usage?.prompt_tokens ?? 0;
  const hits = usage?.prompt_tokens_details?.cached_tokens ?? 0;
  const completion = usage?.completion_tokens ?? 0;
  return (Math.max(0, prompt - hits) * input + hits * cached + completion * output) / 1e6;
}

/** The budget day (UTC). */
export const day = (now = new Date()) => now.toISOString().slice(0, 10);
