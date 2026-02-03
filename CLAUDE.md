# Riff Server

Rust backend + macOS menu bar app for the Riff music server.

## Project Structure

```
riff-server/
├── riff-core/          # Rust library (library scanner, metadata, auth, database)
├── riff-server/        # Rust binary (Axum HTTP server, routes, middleware)
├── RiffApp/            # Swift macOS menu bar app
├── Cargo.toml          # Workspace root
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

## Server Restart

After any change to `riff-core/` or `riff-server/` that affects runtime behavior (routes, config, enrichment logic, etc.), restart the dev server:

```bash
lsof -ti :8080 | xargs kill -9 2>/dev/null
sleep 1
cd /Users/amaslar/riff/riff-server && cargo run -p riff-server 2>&1 &
```

Wait ~5 seconds for it to start, then verify:
```bash
lsof -ti :8080 >/dev/null 2>&1 && echo "server running" || echo "failed"
```

## Commit Scopes

`core`, `server`, `macos`, `docs`

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
