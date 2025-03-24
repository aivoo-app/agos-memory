# Security Policy

## Supported versions

| Version | Supported |
|---------|-----------|
| 0.1.x   | yes       |

## Reporting a vulnerability

Do **not** open a public issue. Email the maintainers (see the repository
owner profile) with:

- affected version / commit
- reproduction steps or PoC
- impact assessment

We aim to acknowledge within 72 hours and will coordinate a disclosure
timeline with the reporter.

## Security-relevant surface (review priorities)

- Bind address & bearer token handling (`src/config.rs`, `src/server` in
  v0.5.0): non-loopback binds fail closed without a token >= 16 chars.
- Secrets redaction: `Config`'s manual `Debug` impl is the sanctioned way to
  log configuration; never log raw keys.
- Hard delete & tombstones (`forget`): leak tests must cover recall, FTS,
  vector, archive, export, and snapshot paths.
- Trust/provenance: untrusted memories are never injected by default.
- Database process lock: one writer per database.
