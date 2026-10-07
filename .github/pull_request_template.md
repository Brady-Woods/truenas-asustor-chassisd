## What and why

<!-- What does this change, and why? Link any issue (e.g. "Fixes #12"). -->

## Testing

<!-- How was it tested? Unit tests, and for hardware changes: NAS model,
TrueNAS SCALE version, kernel, platform driver version. -->

## Checklist

- [ ] Title follows [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/) (e.g. `fix(led): ...`)
- [ ] `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` pass (CI runs them)
- [ ] `deploy.sh` changes stay POSIX `sh`, idempotent, and work unattended as a Post Init script (no `HOME`, no Docker at boot)
- [ ] `CHANGELOG.md` updated under `[Unreleased]` if users would notice
- [ ] README / `lcm-status.example.toml` updated if behaviour, config keys or requirements changed
