# Security policy

## Reporting a vulnerability

Please report security problems privately via
[GitHub security advisories](https://github.com/ymihere/seatswap/security/advisories/new), not
in public issues. You will get an answer within a week.

## Safety model

seatswap handles Claude login tokens, so it is built to keep them where they belong:

- **Tokens stay local.** Stored logins live in the system keyring (Secret Service on Linux,
  Credential Manager on Windows). Without a keyring they go into `0600` files in a `0700` data
  folder, and the pop-up warns about it.
- **Tokens go only to Anthropic.** The only network calls are the usage endpoint
  (`api.anthropic.com`) and Claude Code's token endpoint (`platform.claude.com`), both over HTTPS.
- **No token in output.** Logs, error messages and command output never contain a token; at most
  the last four characters of a refresh token.
- **Minimal writes.** A switch rewrites only the login entries of `.credentials.json` and the
  `oauthAccount` entry of `.claude.json`. Every other key stays exactly as it was. Every write is
  atomic (temporary file, then rename), and the first write keeps a `.seatswap-backup` copy.
- **Locks.** A switch holds Claude Code's own lock files and its own app lock, so it never
  interleaves with a token refresh or a settings write by Claude Code.
- **No shell.** The app runs no commands; the autostart entry is a plain desktop file.

## Scope

Bugs in Claude Code itself, or in Anthropic's endpoints, are out of scope; report those to
Anthropic.
