# Security Policy

Asale relays other people's AI subscription quota, so a bug here can leak credentials or spend someone's money rather than just crash an app. Reports are welcome and taken seriously.

## Reporting a vulnerability

Please **do not open a public issue** for anything security-sensitive.

Use GitHub's private reporting instead: [Security → Report a vulnerability](https://github.com/asale-ai/asale/security/advisories/new). It stays private between you and the maintainers until a fix ships. If you cannot use GitHub advisories, email asale.user@gmail.com instead.

What helps most: the client version, your OS, what you expected, what actually happened, and the smallest set of steps that reproduces it. Please strip any real credentials or tokens out of the report before sending it.

We aim to acknowledge a report within 3 working days, and to ship a fix or publish a mitigation plan within 30 days. If you would like credit in the release notes, say so and we will name you; if you would rather stay anonymous, we will not.

## Supported versions

The client updates itself, and only the latest published release is supported. Everything is still `0.x`: the layout of the local database, the config files and the stored credential references can change between minor versions.

## Scope

In scope is this repository — the desktop client, the `asale` command line, the `asaled` service and the local proxy your CLI talks to. Anything that lets someone read another user's credentials, spend another user's balance, forge a quota authorization, or reach the local proxy from off the machine is worth reporting.

The hosted platform at asale.ai is a separate system. Report those the same way, and say which one you mean.

## Known, and by design

A relayed request is ultimately forwarded to the upstream provider by another user's client, and at that moment the request body is visible to that client. There is no end-to-end encryption and there cannot be, given how the network works. This is a documented limitation rather than a vulnerability — do not send confidential material through the network.

Sharing subscription capacity may also violate an upstream provider's terms of service. That is a product risk, described in the README, not a security bug.
