# Security Policy

## Supported Versions

Only the latest version of this repository is actively maintained.

## Reporting a Vulnerability

Please do not report security vulnerabilities through public GitHub issues.

Send a description of the issue to:

**muthmann@physik.uni-bielefeld.de**

Include:

- A description of the vulnerability and its potential impact
- Steps to reproduce or a minimal proof of concept
- The version or commit hash you tested against

You can expect an acknowledgement within a few business days. Once the issue is confirmed and a fix is available, a coordinated disclosure will be arranged.

## Scope

This repository contains analysis plugin crates for `augur-rs`. Relevant security concerns include:

- Malformed or malicious `plugin.toml` files that could affect the host application
- Unsafe code in plugin crates that could be exploited through crafted input data
- Supply chain issues in workspace dependencies
