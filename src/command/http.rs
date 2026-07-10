//! Shared HTTP client construction with za and environment proxy support.

use crate::command::za_config::{self, ProxyScope};
use anyhow::{Context, Result};
use reqx::{
    advanced::{ClientProfile, RedirectPolicy},
    blocking::{Client, ClientBuilder},
    prelude::RetryPolicy,
};
use std::{env, time::Duration};

const HTTPS_PROXY_ENV_KEYS: [&str; 6] = [
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
    "HTTP_PROXY",
    "http_proxy",
];
const HTTP_PROXY_ENV_KEYS: [&str; 4] = ["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"];

pub fn build_client(
    base_url: &str,
    client_name: &str,
    profile: ClientProfile,
    follow_redirects: bool,
    timeout: Duration,
    proxy_scope: ProxyScope,
) -> Result<Client> {
    let mut builder = Client::builder(base_url)
        .profile(profile)
        .request_timeout(timeout)
        .total_timeout(timeout)
        .retry_policy(RetryPolicy::disabled())
        .client_name(client_name);
    if follow_redirects {
        builder = builder.redirect_policy(RedirectPolicy::follow());
    }
    let scheme = base_url
        .split_once("://")
        .map(|(scheme, _)| scheme)
        .unwrap_or("https");
    apply_proxy(builder, scheme, proxy_scope)
        .with_context(|| format!("configure HTTP client proxy for `{base_url}`"))?
        .build()
        .with_context(|| format!("build HTTP client for `{base_url}`"))
}

pub(crate) fn apply_proxy(
    mut builder: ClientBuilder,
    scheme: &str,
    proxy_scope: ProxyScope,
) -> Result<ClientBuilder> {
    let overrides = za_config::load_proxy_overrides(proxy_scope)?;
    let configured = if scheme.eq_ignore_ascii_case("https") {
        overrides
            .https_proxy
            .or(overrides.all_proxy)
            .or(overrides.http_proxy)
    } else {
        overrides
            .http_proxy
            .or(overrides.all_proxy)
            .or(overrides.https_proxy)
    };
    let proxy_env_keys = proxy_env_keys_for_scheme(scheme);
    let (source, proxy) = configured
        .map(|value| ("za config".to_string(), value))
        .or_else(|| first_env_value(proxy_env_keys))
        .unzip();
    if let Some(proxy) = proxy {
        let proxy_uri = proxy
            .parse()
            .with_context(|| format!("invalid proxy URI from {}", source.unwrap_or_default()))?;
        builder = builder.http_proxy(proxy_uri);
    }

    let no_proxy = overrides
        .no_proxy
        .or_else(|| first_env_value(&["NO_PROXY", "no_proxy"]).map(|(_, value)| value));
    if let Some(no_proxy) = no_proxy {
        let rules = split_no_proxy_rules(&no_proxy);
        if !rules.is_empty() {
            builder = builder
                .try_no_proxy(rules)
                .context("invalid NO_PROXY rules")?;
        }
    }
    Ok(builder)
}

pub(crate) fn proxy_env_keys_for_scheme(scheme: &str) -> &'static [&'static str] {
    if scheme.eq_ignore_ascii_case("https") {
        &HTTPS_PROXY_ENV_KEYS
    } else {
        &HTTP_PROXY_ENV_KEYS
    }
}

pub(crate) fn split_no_proxy_rules(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|rule| !rule.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn first_env_value(names: &[&str]) -> Option<(String, String)> {
    names.iter().find_map(|name| {
        let value = env::var(name).ok()?;
        let value = value.trim();
        (!value.is_empty()).then(|| ((*name).to_string(), value.to_string()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_proxy_values_are_ignored() {
        assert_eq!(first_env_value(&["ZA_HTTP_TEST_MISSING"]), None);
    }
}
