//! GB35114 product-glue tests. The disabled-path and validation tests run
//! on every build; the enabled path (real SM2 authenticator + handshake
//! smoke) only when the `gb35114` feature is compiled in.

use mibee_eye_raspi_rs::config::Gb35114Config;
use mibee_eye_raspi_rs::gb35114_glue;

fn enabled_cfg() -> Gb35114Config {
    Gb35114Config {
        enabled: true,
        device_cert_file: format!(
            "{}/tests/fixtures/gb35114/device_cert.pem",
            env!("CARGO_MANIFEST_DIR")
        ),
        device_key_file: format!(
            "{}/tests/fixtures/gb35114/device_key.pem",
            env!("CARGO_MANIFEST_DIR")
        ),
        platform_cert_file: format!(
            "{}/tests/fixtures/gb35114/platform_cert.pem",
            env!("CARGO_MANIFEST_DIR")
        ),
        server_id: "34020000002000000001".to_string(),
    }
}

#[test]
fn disabled_returns_none() {
    let auth = gb35114_glue::build(&Gb35114Config::default(), "34020000001320000001").unwrap();
    assert!(auth.is_none());
}

#[test]
fn validation_rejects_incomplete_config() {
    let mut cfg = enabled_cfg();
    cfg.server_id = String::new();
    assert!(gb35114_glue::build(&cfg, "34020000001320000001").is_err());

    let mut cfg = enabled_cfg();
    cfg.device_cert_file = String::new();
    assert!(gb35114_glue::build(&cfg, "34020000001320000001").is_err());

    assert!(gb35114_glue::build(&enabled_cfg(), "").is_err());
}

#[cfg(feature = "gb35114")]
mod enabled {
    use super::*;
    use gb28181_rs::security35114 as sec;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/gb35114/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    const DEVICE_ID: &str = "34020000001320000001";
    const SERVER_ID: &str = "34020000002000000001";

    #[test]
    fn enabled_builds_authenticator() {
        let auth = gb35114_glue::build(&enabled_cfg(), DEVICE_ID)
            .unwrap()
            .expect("authenticator with feature + enabled config");
        assert!(auth.initial_authorization().starts_with("Capability "));
    }

    #[test]
    fn bad_cert_file_is_error() {
        let mut cfg = enabled_cfg();
        cfg.device_cert_file = "/nonexistent.pem".to_string();
        assert!(gb35114_glue::build(&cfg, DEVICE_ID).is_err());
    }

    // One full A-level handshake through the product-built authenticator:
    // challenge → signed re-REGISTER → platform-side sign1 verification
    // with the provisioned identity. Pins the product wiring; the library
    // has its own end-to-end suite.
    #[test]
    fn handshake_smoke() {
        let auth = gb35114_glue::build(&enabled_cfg(), DEVICE_ID)
            .unwrap()
            .expect("authenticator");
        let authz = auth
            .authorize_with_challenge(
                "Unidirection algorithm=\"A:SM2;H:SM3;S:SM4/OFB/PKCS5;SI:SM3-SM2\", random1=\"PRAIIbutDbd5x/NKsbwwYw==\"",
            )
            .unwrap();
        let aa = sec::parse_auth_authorization(&authz).unwrap();
        let identity =
            sec::load_identity(&fixture("device_cert.pem"), &fixture("device_key.pem")).unwrap();
        let payload = sec::sign_auth_payload(
            &aa.random1,
            &aa.random2,
            SERVER_ID,
            sec::RandomEncoding::ConcatWireStrings,
        );
        sec::verify_message(&identity.certificate, &payload, &aa.sign1)
            .expect("sign1 from product authenticator verifies");
    }
}
