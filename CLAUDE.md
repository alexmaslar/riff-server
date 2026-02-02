# Riff Server

A self-hosted music server (macOS menu bar app) for audiophiles. Rust backend + Swift macOS wrapper.

## Documentation

- [API Reference](./docs/API.md) — REST endpoints, auth, data model

## Project Structure

```
riff-server/
├── riff-core/          # Rust library (library scanner, metadata, auth, database)
├── riff-server/        # Rust binary (Axum HTTP server, routes, middleware)
├── RiffApp/            # Swift macOS menu bar app
├── Cargo.toml          # Workspace root
├── docs/
│   └── API.md
├── LICENSE
└── README.md
```

## Tech Stack

| Component | Stack |
|-----------|-------|
| Core library | Rust, SQLite, Symphonia (audio decoding) |
| HTTP server | Rust, Axum, Tower |
| macOS app | Swift, SwiftUI |
| Auth | JWT (jsonwebtoken crate) |

## Common Commands

```bash
cargo build
cargo run -p riff-server
cargo test
cargo clippy
```

## Git Workflow

**Branching:**
- `main` — Stable, release-ready
- `feature/<name>` — New features
- `fix/<name>` — Bug fixes

**Commits:**
- Use conventional commits: `type(scope): message`
- Types: `feat`, `fix`, `refactor`, `docs`, `test`, `chore`
- Scope: `core`, `server`, `macos`, `docs`
- Keep commits small and focused

Examples:
```
feat(core): add library scanner with FLAC support
fix(server): handle missing album art in stream response
refactor(core): extract metadata parsing into separate module
```

## Code Organization

- Keep `riff-core` (library) separate from `riff-server` (binary) for testability
- Scanner lives in `riff-core/src/scanner/`
- Routes live in `riff-server/src/routes/`
- Database migrations in `riff-core/migrations/`

## Key Decisions

- **No transcoding** — Serve FLAC/ALAC as-is, clients handle decoding
- **Discogs for metadata** — Primary source; AI summaries are supplemental
- **SQLite** — Single-file database, no external DB dependency
- **Axum** — Async web framework with Tower middleware
