//! What a desktop may ask for and what it costs: allowed models and limits,
//! cost from the provider's token counts, and per-desktop daily usage in
//! DynamoDB (the same table and items the Lambda gateway kept).

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::{Value, json};

use crate::aws::Aws;
use crate::config::Config;

pub const MAX_TOKENS: u64 = 16_384;
/// Usage items expire after 90 days.
const KEEP_SECS: u64 = 90 * 86_400;

/// Checks a chat request: an allowed model, a messages list, no streaming,
/// max_tokens within the limit. Returns the model's prices.
pub fn check(request: &Value, prices: &HashMap<String, [f64; 3]>) -> Result<[f64; 3], String> {
    let model = request["model"].as_str().unwrap_or_default();
    let prices = *prices
        .get(model)
        .ok_or_else(|| format!("model not allowed: {model}"))?;
    if !request["messages"].is_array() {
        return Err("messages must be a list".into());
    }
    if request["stream"].as_bool() == Some(true) {
        return Err("streaming isn't supported".into());
    }
    if let Some(max) = request.get("max_tokens").filter(|v| !v.is_null()) {
        match max.as_u64() {
            Some(n) if (1..=MAX_TOKENS).contains(&n) => {}
            _ => return Err(format!("max_tokens must be 1 to {MAX_TOKENS}")),
        }
    }
    Ok(prices)
}

/// USD for a reply's usage, with [input, cached input, output] per million tokens.
pub fn cost(usage: &Value, [input, cached, output]: [f64; 3]) -> f64 {
    let prompt = usage["prompt_tokens"].as_u64().unwrap_or(0);
    let hit = usage["prompt_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap_or(0)
        .min(prompt);
    let completion = usage["completion_tokens"].as_u64().unwrap_or(0);
    ((prompt - hit) as f64 * input + hit as f64 * cached + completion as f64 * output) / 1e6
}

/// Today in UTC, YYYY-MM-DD.
pub fn day() -> String {
    let days = (unix_now() / 86_400) as i64;
    // Civil from days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn key(desktop: &str, day: &str) -> Value {
    json!({"desktop": {"S": desktop}, "day": {"S": day}})
}

/// USD this desktop has spent today.
pub async fn spent(aws: &Aws, cfg: &Config, desktop: &str, day: &str) -> Result<f64> {
    let answer = aws
        .call(
            "dynamodb",
            &cfg.region,
            "DynamoDB_20120810.GetItem",
            &json!({"TableName": cfg.usage_table, "Key": key(desktop, day), "ProjectionExpression": "cost"}),
        )
        .await?;
    Ok(answer["Item"]["cost"]["N"]
        .as_str()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0.0))
}

pub async fn record(
    aws: &Aws,
    cfg: &Config,
    desktop: &str,
    day: &str,
    cost: f64,
    (input, output): (u64, u64),
) -> Result<()> {
    aws.call(
        "dynamodb",
        &cfg.region,
        "DynamoDB_20120810.UpdateItem",
        &json!({
            "TableName": cfg.usage_table,
            "Key": key(desktop, day),
            "UpdateExpression": "ADD cost :c, input_tokens :i, output_tokens :o, calls :one SET expires = :x",
            "ExpressionAttributeValues": {
                ":c": {"N": format!("{cost:.6}")},
                ":i": {"N": input.to_string()},
                ":o": {"N": output.to_string()},
                ":one": {"N": "1"},
                ":x": {"N": (unix_now() + KEEP_SECS).to_string()},
            },
        }),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prices() -> HashMap<String, [f64; 3]> {
        HashMap::from([("zai-org/GLM-5.3".to_string(), [1.40, 0.26, 4.40])])
    }

    #[test]
    fn only_allowed_bounded_requests_pass() {
        let ok = json!({"model": "zai-org/GLM-5.3", "messages": [], "max_tokens": 8192});
        assert_eq!(check(&ok, &prices()), Ok([1.40, 0.26, 4.40]));
        for bad in [
            json!({"model": "other/model", "messages": []}),
            json!({"model": "zai-org/GLM-5.3"}),
            json!({"model": "zai-org/GLM-5.3", "messages": [], "stream": true}),
            json!({"model": "zai-org/GLM-5.3", "messages": [], "max_tokens": 100_000}),
            json!({"model": "zai-org/GLM-5.3", "messages": [], "max_tokens": "lots"}),
        ] {
            assert!(check(&bad, &prices()).is_err(), "{bad}");
        }
    }

    #[test]
    fn costs_count_cached_input_at_its_price() {
        let usage = json!({"prompt_tokens": 2516, "completion_tokens": 126, "prompt_tokens_details": {"cached_tokens": 0}});
        assert!((cost(&usage, [1.40, 0.26, 4.40]) - 0.004_076_8).abs() < 1e-9);
        let cached = json!({"prompt_tokens": 1_000_000, "completion_tokens": 0, "prompt_tokens_details": {"cached_tokens": 1_000_000}});
        assert!((cost(&cached, [1.40, 0.26, 4.40]) - 0.26).abs() < 1e-9);
        assert_eq!(cost(&Value::Null, [1.0, 1.0, 1.0]), 0.0);
    }

    #[test]
    fn days_are_utc_dates() {
        let today = day();
        assert_eq!(today.len(), 10);
        assert!(today.starts_with("20"));
    }
}
