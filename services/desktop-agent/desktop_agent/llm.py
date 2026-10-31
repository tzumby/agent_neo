"""Together AI chat completions (OpenAI-compatible API) over plain HTTPS."""

import json
import time
import urllib.error
import urllib.request

API_URL = "https://api.together.ai/v1/chat/completions"

# USD per 1M tokens: input, cached input, output (Together serverless, 2026-10-30).
PRICES = {
    "zai-org/GLM-5.3-Flash": (0.15, 0.03, 0.50),
    "zai-org/GLM-5.3": (1.40, 0.26, 4.40),
    "moonshotai/Kimi-K3": (3.00, 0.30, 15.00),
}

RETRYABLE = (429, 500, 502, 503, 504)


class LLMError(Exception):
    pass


class Together:
    def __init__(self, api_key):
        self.headers = {
            "Authorization": f"Bearer {api_key}",
            "Content-Type": "application/json",
            # Cloudflare in front of the API rejects urllib's default User-Agent (error 1010).
            "User-Agent": "cloud-desktops-agent/0.1",
        }

    def chat(self, body, log=print):
        """POST a chat completion, retrying rate limits and outages."""
        data = json.dumps(body).encode()
        for attempt in range(6):
            request = urllib.request.Request(API_URL, data=data, headers=self.headers)
            try:
                with urllib.request.urlopen(request, timeout=180) as response:
                    return json.load(response)
            except urllib.error.HTTPError as exc:
                if exc.code not in RETRYABLE or attempt == 5:
                    detail = exc.read().decode(errors="replace")[:500]
                    raise LLMError(f"Together API error {exc.code}: {detail}") from None
                retry_after = exc.headers.get("Retry-After", "")
                delay = (
                    int(retry_after) if retry_after.isdigit() else 2 ** (attempt + 1)
                )
                log(f"HTTP {exc.code} from Together, retrying in {delay}s")
            except (urllib.error.URLError, TimeoutError) as exc:
                if attempt == 5:
                    raise LLMError(f"Cannot reach Together: {exc}") from None
                delay = 2 ** (attempt + 1)
                log(f"{exc} talking to Together, retrying in {delay}s")
            time.sleep(delay)
        raise AssertionError("unreachable")


class Usage:
    def __init__(self):
        self.input = self.cached = self.output = 0

    def add(self, usage):
        usage = usage or {}
        self.input += usage.get("prompt_tokens") or 0
        self.cached += (usage.get("prompt_tokens_details") or {}).get(
            "cached_tokens"
        ) or 0
        self.output += usage.get("completion_tokens") or 0

    def cost(self, model):
        if model not in PRICES:
            return None
        price_in, price_cached, price_out = PRICES[model]
        uncached = self.input - self.cached
        return (
            uncached * price_in + self.cached * price_cached + self.output * price_out
        ) / 1e6

    def describe(self, model):
        dollars = self.cost(model)
        cost = f"≈ ${dollars:.4f}" if dollars is not None else "cost unknown"
        return (
            f"{self.input} input tokens ({self.cached} cached), "
            f"{self.output} output tokens, {cost}"
        )
