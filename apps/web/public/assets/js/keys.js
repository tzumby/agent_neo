// Spots an API key or token pasted into a message, so the app can offer to
// save it on the desktop instead: anything sent in the chat goes to the model
// and stays in the history. The same formats as the apps' KeyDetector.

const KINDS = [
  [/\bgithub_pat_[A-Za-z0-9_]{40,}/, "GITHUB_TOKEN", "GitHub token"],
  [/\bgh[pousr]_[A-Za-z0-9]{30,}/, "GITHUB_TOKEN", "GitHub token"],
  [/\bglpat-[A-Za-z0-9_-]{20,}/, "GITLAB_TOKEN", "GitLab token"],
  [/\bsk-ant-[A-Za-z0-9_-]{20,}/, "ANTHROPIC_API_KEY", "Anthropic API key"],
  [/\btgp_v1_[A-Za-z0-9_-]{20,}/, "TOGETHER_API_KEY", "Together AI API key"],
  [/\b[sr]k_(live|test)_[A-Za-z0-9]{20,}/, "STRIPE_API_KEY", "Stripe API key"],
  [/\bsk-[A-Za-z0-9_-]{20,}/, "OPENAI_API_KEY", "OpenAI API key"],
  [/\bxox[abposr]-[A-Za-z0-9-]{10,}/, "SLACK_TOKEN", "Slack token"],
  [/\bhf_[A-Za-z0-9]{30,}/, "HF_TOKEN", "Hugging Face token"],
  [/\bAIza[0-9A-Za-z_-]{35}/, "GOOGLE_API_KEY", "Google API key"],
  [/\bAKIA[0-9A-Z]{16}\b/, "AWS_ACCESS_KEY_ID", "AWS access key ID"],
];

/** The first well-known key in the text: {value, name, label}, or null. */
export function findKey(text) {
  for (const [re, name, label] of KINDS) {
    const m = text.match(re);
    if (m) return { value: m[0], name, label };
  }
  return null;
}

/** The name a key would be saved as: capitals, digits and underscores. */
export function normalizedName(text) {
  return text.toUpperCase().replace(/[ -]/g, "_").replace(/[^A-Z0-9_]/g, "").slice(0, 64);
}

/** Whether agentd accepts this name (secrets.rs: check_name). */
export function isValidName(name) {
  return /^[A-Z][A-Z0-9_]{1,63}$/.test(name) && !["PASSWORD", "PASSWD", "PASSCODE"].some((w) => name.includes(w));
}
