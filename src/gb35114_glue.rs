//! Product glue for GB 35114 A-level security: bridges the product's
//! `[gb28181.gb35114]` config to the gb28181-rs `security35114`
//! authenticator. Compile-time opt-in like the Go twin's `-tags gb35114`;
//! builds without the feature warn and fall back to SIP Digest.

use std::sync::Arc;

use gb28181_rs::authenticator::RegisterAuthenticator;

/// Builds the REGISTER authenticator from the product config.
///
/// - disabled → `Ok(None)` (Digest flow unchanged)
/// - enabled + `gb35114` feature → `Ok(Some(authenticator))`
/// - enabled, no feature → warning + `Ok(None)` (Digest fallback)
/// - enabled but misconfigured → `Err` — the caller must treat a
///   half-configured security identity as fatal, never silently downgrade.
pub fn build(
    cfg: &crate::config::Gb35114Config,
    device_id: &str,
) -> anyhow::Result<Option<Arc<dyn RegisterAuthenticator>>> {
    if !cfg.enabled {
        return Ok(None);
    }
    if device_id.is_empty() {
        anyhow::bail!("gb35114: gb28181.device_id is required");
    }
    if cfg.server_id.is_empty() {
        anyhow::bail!("gb35114: gb28181.gb35114.server_id is required");
    }
    if cfg.device_cert_file.is_empty() || cfg.device_key_file.is_empty() {
        anyhow::bail!("gb35114: gb28181.gb35114.device_cert_file and device_key_file are required");
    }
    authenticator_shim::new_authenticator(cfg, device_id)
}

#[cfg(feature = "gb35114")]
mod authenticator_shim {
    use super::*;
    use anyhow::Context;
    use gb28181_rs::security35114 as sec;

    pub(super) fn new_authenticator(
        cfg: &crate::config::Gb35114Config,
        device_id: &str,
    ) -> anyhow::Result<Option<Arc<dyn RegisterAuthenticator>>> {
        let identity =
            sec::load_identity(&read(&cfg.device_cert_file)?, &read(&cfg.device_key_file)?)
                .with_context(|| "gb35114: loading device identity")?;
        let mut opts = sec::Options::new(identity, device_id, &cfg.server_id);
        if !cfg.platform_cert_file.is_empty() {
            let platform = sec::load_certificate(&read(&cfg.platform_cert_file)?)
                .with_context(|| "gb35114: loading platform certificate")?;
            opts.platform_cert = Some(platform);
        }
        let auth = sec::Authenticator::new(opts).context("gb35114")?;
        Ok(Some(Arc::new(auth)))
    }

    fn read(path: &str) -> anyhow::Result<String> {
        std::fs::read_to_string(path).with_context(|| format!("reading {path}"))
    }
}

#[cfg(not(feature = "gb35114"))]
mod authenticator_shim {
    use super::*;

    pub(super) fn new_authenticator(
        _cfg: &crate::config::Gb35114Config,
        _device_id: &str,
    ) -> anyhow::Result<Option<Arc<dyn RegisterAuthenticator>>> {
        eprintln!(
            "gb35114: enabled in config but this binary was built without the gb35114 feature — falling back to Digest auth"
        );
        Ok(None)
    }
}
