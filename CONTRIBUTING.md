# Contributing to Super TTS

Thank you for your interest in contributing to Super TTS! This document provides guidelines for contributing to the project.

## Development Setup

### Prerequisites

- A recent stable Rust toolchain (edition 2024).
- The [`just`](https://github.com/casey/just) task runner: `cargo install just`.
- System build dependencies, by distro:

  ```bash
  # Debian/Ubuntu/Pop!_OS
  sudo apt install build-essential libxkbcommon-dev libasound2-dev pkg-config libssl-dev
  # Fedora
  sudo dnf install gcc gcc-c++ libxkbcommon-devel alsa-lib-devel pkgconf perl-FindBin perl-IPC-Cmd openssl-devel
  # Arch
  sudo pacman -S pkgconf openssl
  ```

  If a dependency is missing for your distro, a PR to update this list is welcome.

### Clone and build

```bash
git clone https://github.com/jorge-menjivar/super-tts.git
cd super-tts

just install            # build and install everything, wired to systemd
# …or one piece at a time:
just install-daemon
just install-app
just install-applet     # COSMIC only
```

### Development commands

```bash
just run-daemon         # run the daemon in the foreground
just run-app            # run the settings app
just run-applet         # run the COSMIC applet
just audit              # security audit (cargo audit)
```

## Workspace layout

Super TTS is a Rust workspace:

| Crate                      | Role                                                              |
|----------------------------|------------------------------------------------------------------|
| `super-tts-daemon`         | The engine: installs backends, loads models, serves the protocol |
| `super-tts-app`            | Desktop settings & management app                                |
| `super-tts-cli`            | The `tts` command-line client                                    |
| `super-tts-cosmic-applet`  | COSMIC panel applet with visualizations                          |
| `super-tts-consent`        | Consent-popup helper for the auth handshake                      |
| `super-tts-shared`         | Common types, protocol definitions, validation                   |
| `super-tts-registry-types` | Shared backend registry / manifest types                         |
| `super-tts-forge`          | Git-forge release sourcing for the registry                      |
| `super-tts-indexer`        | CI tool that builds the published registry `index.json`          |

The protocol and backend contract that clients and backend authors build
against live in [`docs/protocol/`](./docs/protocol/).

## Code Style and Standards

- **Rust**: Follow standard Rust conventions and use `cargo fmt`
- **Security**: All external inputs must be validated using the shared validation framework
- **Testing**: Add tests for new functionality, especially security-critical code
- **Documentation**: Document public APIs and security-relevant functions

## Security Guidelines

- Never bypass the process authentication system
- All network communication must validate inputs
- Use the shared validation framework in `super-tts-shared/src/validation/`
- Follow the development vs production security model (debug vs release builds)
- Run security audits before proposing changes: `cargo audit`

## Pull Request Process

1. **Before submitting**:
   - Run `cargo test` to ensure all tests pass

     > The integration suites spawn a real daemon, so they set
     > `SUPER_TTS_MUTE_CUES=1` on it — otherwise auditioning the audio theme
     > (`POST /audio_theme/test`) plays beeps out of your speakers on every
     > run. If you add a harness that spawns the daemon binary, set it too.
   - Run `cargo fmt` to format code
   - Run `cargo clippy` to check for warnings
   - Run `cargo audit` to check for security vulnerabilities
   - Test on both debug and release builds

2. **Pull Request Requirements**:
   - Clear description of changes
   - Reference any related issues
   - Include tests for new functionality
   - Update documentation if needed

3. **Review Process**:
   - All PRs require review
   - Security-related changes require additional scrutiny
   - CI must pass before merging

## Reporting Security Issues

If you discover a security vulnerability, please:

1. **Do not** open a public issue
2. Email security concerns to: jorge@menjivar.ai
3. Include detailed reproduction steps
4. Allow reasonable time for response before public disclosure

## Code of Conduct

- Be respectful and inclusive
- Focus on constructive feedback
- Help maintain a welcoming environment for all contributors

## License

By contributing to Super TTS, you agree that your contributions will be licensed under the GPL-3.0-only license.

## Questions?

- Open an issue for feature requests or bugs
- Join discussions in existing issues
- Contact: jorge@menjivar.ai
