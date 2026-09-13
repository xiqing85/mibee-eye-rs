# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability in this project, please **report it via GitHub Issues** on the shared MiBee Eye tracker:

1. Go to [xiqing85/mibee-eye-go/issues/new](https://github.com/xiqing85/mibee-eye-go/issues/new)
2. Set the title prefix `[security]` (plus `[rs]` for this implementation) and apply the `security` label
3. Provide a clear description of the issue and steps to reproduce

Do **not** open public issues for critical vulnerabilities — use the security label to help maintainers triage appropriately.

We aim to acknowledge reports within 5 business days and address them on a best-effort basis. As a community project, there is no guaranteed SLA.

## Supported Versions

Only the **latest release** is supported with security updates. Always upgrade to the newest version promptly.

| Version | Supported |
|---------|-----------|
| Latest  | ✅        |
| Older   | ❌        |

## Security Best Practices

When deploying this camera service, follow these guidelines:

- **Change default credentials** — set `onvif.password` (and web credentials) before exposing the service to any network
- **Restrict network access** — use firewall rules to limit access to camera ports (RTSP: 8554, ONVIF: 8080, Web UI: 8088) only to trusted NVRs or clients
- **Run with least privilege** — avoid running as root; the provided systemd unit uses a dedicated user
- **Isolate the camera network** — place the device on a separate VLAN if possible
- **Keep the service updated** — always run the latest release
