# Repository guidelines

Use semantic commits (Conventional Commits), for example `feat: add camera discovery` or `fix: handle stream disconnects`.

## Crate separation

- Split crates around real responsibilities, dependency boundaries, and measured compilation benefits; avoid speculative or empty crates and needless fragmentation.
- Keep protocol and device logic independent of GTK, media rendering, and HTTP server dependencies. Dependencies must flow toward shared services, not from them into frontends.
- Keep dependencies and enabled features as narrow as practical so individual crates can be checked and tested without building unrelated subsystems.
- Place shared types with the behavior that owns them; introduce a separate shared crate only when a real dependency boundary requires it.
- Measure build performance before optimizing crate boundaries.
