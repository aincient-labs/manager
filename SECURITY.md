# Security Policy

The `atelier` CLI and Manager GUI drive the Docker appliance that runs people's websites —
install, update, back up, restore. A vulnerability here can affect real sites, so we take
security reports seriously. Thank you for helping keep Atelier safe.

## Reporting a vulnerability

**Please do not report security issues in public GitHub issues, pull requests, or
discussions.**

Report privately through **GitHub's private vulnerability reporting**:

1. Go to the **[Security tab](https://github.com/aincient-labs/manager/security)** of this
   repository.
2. Click **Report a vulnerability**.
3. Fill in the advisory form.

This opens a private channel visible only to you and the maintainers. A good report includes:

- The version (`atelier --version`) and how you installed it (Homebrew, direct download).
- Your OS and architecture.
- A clear description of the issue and its impact.
- Steps to reproduce, ideally with a minimal proof of concept.

## What to expect

- **Acknowledgement** within **3 business days**.
- An **initial assessment** (severity, reproducibility, rough timeline) within **7 business
  days**.
- Regular updates, and **credit** in the advisory and release notes when the fix ships,
  unless you'd prefer to stay anonymous.

We follow **coordinated disclosure**: we agree a disclosure date with you, publish a GitHub
Security Advisory when the fix is available, and ask that details stay private until then.

## Supported versions

The manager follows **[Semantic Versioning](https://semver.org/)** and is released through
[GitHub Releases](https://github.com/aincient-labs/manager/releases) and the
`aincient-labs/homebrew-tap`. Security fixes ship in a new release; the **latest release** is
the supported version. Update with `brew upgrade atelier` or by downloading the newest release.

## Verifying what you run

Release binaries are code-signed:

- **macOS** builds are **Developer ID signed and notarized** by Apple.
- **Windows** builds are signed (via SignPath).

Your OS verifies these signatures on first launch. If a build fails Gatekeeper or SmartScreen
verification, don't run it — open a security report instead.

## Scope

Anything in this repository is in scope: the CLI, the Tauri GUI, and the shared
`aincient-core` Rust crate that drives the Docker lifecycle. Issues in the **CMS appliance
itself** (the container image, operator console, or modules) belong in the
[atelier-cms security channel](https://github.com/aincient-labs/atelier-cms/security).

Thank you for helping keep Atelier and the sites it runs safe.
