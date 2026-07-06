# Security Policy

## Supported versions

Only the latest release receives security fixes.

## Reporting a vulnerability

Please report vulnerabilities **privately** — do not open a public issue.

Use [GitHub private vulnerability reporting](https://github.com/l0ng-ai/tty7/security/advisories/new)
— "Report a vulnerability" under the repository's **Security** tab.

You should get an initial response within a few days. Please include a
reproduction if you can — a byte sequence, a clipboard payload, or a shell
snippet is ideal.

## Scope notes

A terminal emulator's attack surface is unusual: untrusted input arrives as
escape sequences from anything you `cat`, `ssh`, or paste. Reports in these
areas are especially valuable:

- Escape-sequence parsing (VT/OSC/CSI handling, including the daemon-side
  scanners).
- Clipboard and paste handling (e.g. bracketed-paste escapes).
- Shell-integration scripts and the `ZDOTDIR` bootstrap.
- The daemon's Unix socket / named pipe protocol and its process lifecycle.
