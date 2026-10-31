//! Settings that don't affect what the gateway can see, from the instance's
//! user data (JSON, set by Terraform): who may call, where usage goes, prices,
//! the budget. What does matter (the provider's address, what's logged, the
//! TLS key and attestation) is compiled in, so it's covered by attestation.

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::aws::{Aws, valid_region};

#[derive(Deserialize)]
pub struct Config {
    #[serde(skip)]
    pub region: String,
    pub account_id: String,
    /// Desktop roles are this prefix, or the prefix and "-<user>".
    pub desktop_role_prefix: String,
    /// The value desktops sign into their identity proof for this gateway.
    pub audience: String,
    pub usage_table: String,
    /// The SSM SecureString holding the provider's API key.
    pub key_parameter: String,
    pub daily_budget_usd: f64,
    /// Model → [input, cached input, output] in USD per million tokens. Only
    /// these models are allowed.
    pub prices: HashMap<String, [f64; 3]>,
}

impl Config {
    /// From the user data, or GATEWAYD_CONFIG and GATEWAYD_REGION when run
    /// outside EC2 (development: the image's unit sets neither).
    pub async fn load(aws: &Aws) -> Result<Self> {
        let text = match std::env::var("GATEWAYD_CONFIG") {
            Ok(path) => std::fs::read_to_string(path)?,
            Err(_) => aws.metadata("user-data").await.context("reading the user data")?,
        };
        let mut config: Config =
            serde_json::from_str(&text).context("the user data isn't the gateway's JSON")?;
        config.region = match std::env::var("GATEWAYD_REGION") {
            Ok(region) => region,
            Err(_) => aws.metadata("meta-data/placement/region").await?,
        };
        valid_region(&config.region)?;
        Ok(config)
    }
}
