# Contributing

Thanks for taking an interest. This is a small, single-maintainer project;
issues and pull requests are welcome.

## Scope

This repo holds `lcm-status`, the daemon that drives an ASUSTOR NAS's
front-panel LCD, status/bay/NIC LEDs, buzzer and fans under TrueNAS SCALE,
plus its `deploy.sh` (build, install, and re-run as a Post Init script on
every boot).

Not here, so please report them in the component's own repo:

- kernel driver behaviour (LED class devices, GPIO, `it87`, the buzzer
  device, `lcd_power`, EuP):
  [Brady-Woods/asustor-platform-driver](https://github.com/Brady-Woods/asustor-platform-driver)

## Reporting a problem

Please use the bug report form and include:

- TrueNAS SCALE version, kernel (`uname -r`) and NAS model;
- the version of `lcm-status` (tag or commit) and of the platform driver
  fork;
- the output of `lcm-status status`;
- the relevant part of `journalctl -u lcm-status` (e.g. `-b` for the
  current boot);
- your `/etc/lcm-status.toml` if the problem may depend on it.

Remove anything you consider private (host names, pool names) before
posting.

## Commits

Commit messages follow [Conventional Commits 1.0.0](https://www.conventionalcommits.org/en/v1.0.0/):

```
<type>[optional scope]: <description>

[optional body]
```

Types used here: `feat`, `fix`, `docs`, `style`, `refactor`, `perf`,
`test`, `ci`, `chore`. Scopes are optional and name the area, e.g. `led`,
`lcd`, `fan`, `buzzer`, `deploy`: `fix(led): re-apply LED settings when
the platform driver reloads`. Mark breaking changes (config keys, a newer
minimum driver version, socket protocol) with `!` or a `BREAKING CHANGE:`
footer. Explain *why* in the body, especially for anything found on real
hardware.

Add a line under `## [Unreleased]` in `CHANGELOG.md` for anything a user
would notice.

## Rust

- Edition 2024, `rust-version` in `Cargo.toml`; dependencies kept to a
  minimum (the release binary is a static musl build).
- Format with `cargo fmt`.
- Lint with `cargo clippy --all-targets -- -D warnings`. Clippy
  `pedantic` is on (`[lints]` in `Cargo.toml`, `clippy.toml` for
  `doc-valid-idents`); allow a lint locally with a reason rather than
  globally.
- Add or update unit tests (`cargo test`) for logic that doesn't need the
  hardware: config parsing, protocol framing, state and alarm decisions,
  schedules.
- Hardware access goes through the platform driver's sysfs and input
  devices, hwmon and the LCD's serial port, not raw I/O ports or GPIO
  exports (see 2.0.0 in the changelog for why).

CI runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
`cargo test`, `sh -n deploy.sh` and `cargo deny check advisories` (RustSec
advisories, see `deny.toml`) on every push and pull request. GitHub Actions
are pinned by commit SHA with a `# vX` comment; Dependabot proposes updates.
Changes that touch hardware should also be tried on a NAS; say which model
and TrueNAS version in the pull request.

## deploy.sh

- POSIX `sh` (`#!/bin/sh`, `set -eu`); no bashisms.
- It runs **unattended as a TrueNAS Post Init script**: as root, with no
  `HOME`, no TTY and a minimal environment, before the Docker daemon is up,
  and possibly before the platform driver's own Post Init script has
  loaded the modules. Never rely on `$HOME`, prompts, or Docker being
  available at boot (an unchanged checkout must not need Docker at all).
- **Idempotent**: re-running must be safe and change nothing when
  everything is already in place; never overwrite an existing
  `/etc/lcm-status.toml`.
- Nothing may be installed under `/usr` (read-only in a fresh boot
  environment); persistent state belongs in the TrueNAS config database or
  next to the checkout on the data pool.
- Test both ways, and twice (the second run should change nothing):

  ```sh
  sudo ./deploy.sh
  sudo env -i PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
      ./deploy.sh </dev/null
  ```

## Versioning and releases

Releases follow [Semantic Versioning](https://semver.org/): **major** for
changes that need a newer platform driver, a manual step, or break config
or the socket protocol; **minor** for new features; **patch** for fixes.

Release process (maintainer):

1. Move the `[Unreleased]` entries in `CHANGELOG.md` to a new
   `## [X.Y.Z] - YYYY-MM-DD` section and update the compare links.
2. Set `version` in `Cargo.toml` and refresh `Cargo.lock`
   (`cargo update -p lcm-status --offline`).
3. Commit: `chore(release): X.Y.Z`.
4. Tag it, annotated, with that changelog section as the message:
   `git tag -a vX.Y.Z --cleanup=verbatim -F notes.md` (verbatim keeps the
   `###` headings).
5. Push `main`, then the tag on its own (GitHub starts no workflows when
   more than three tags are pushed at once).
6. The release workflow builds a static `x86_64-unknown-linux-musl`
   binary and attaches it (tarball and SHA-256) to the GitHub release for
   the tag, creating the release if it doesn't exist yet. Put the release
   notes on it (`gh release edit vX.Y.Z --notes-file notes.md`).
