# Riff Server

A self-hosted music server for audiophiles. Serves your FLAC/ALAC library over a REST API with Discogs metadata enrichment and AI-powered album summaries. Runs as a macOS menu bar app.

**No subscriptions. No cloud. Your music, your server.**

## Features

- **Library scanner** — Watches your music directory, extracts metadata from FLAC, ALAC, WAV, and AIFF files
- **REST API** — Browse artists, albums, tracks; stream and download audio
- **Discogs integration** — Enriches your library with genres, styles, labels, credits, and cover art
- **AI album summaries** — Generate album overviews using OpenAI, Anthropic, or Ollama
- **Multi-user auth** — JWT-based authentication with admin and user roles
- **Playlists** — Server-side playlist storage with sharing
- **macOS menu bar app** — Native Swift wrapper with preferences UI, auto-start on login
- **Focus filters** — Roon-style dynamic filtering by genre, decade, format, label, and more

## Architecture

```
riff-server/
├── riff-core/       # Rust library — scanner, metadata, auth, database
├── riff-server/     # Rust binary — Axum HTTP server, routes, middleware
└── RiffApp/         # Swift macOS menu bar app wrapping the Rust binary
```

| Component | Stack |
|-----------|-------|
| Core library | Rust, SQLite, Symphonia |
| HTTP server | Rust, Axum, Tower |
| macOS app | Swift, SwiftUI |
| Auth | JWT (jsonwebtoken) |

## Getting Started

### Prerequisites

- Rust 1.75+ (`rustup`)
- Xcode 15+ (for macOS app)
- A music library (FLAC/ALAC files)

### Build & Run

```bash
# Build the server
cargo build --release

# Run the server
cargo run --release -p riff-server

# Run tests
cargo test

# Lint
cargo clippy
```

The server starts on `http://localhost:8080` by default.

### Configuration

Create `~/.config/riff/config.yaml`:

```yaml
server:
  port: 8080

library:
  path: /path/to/your/music
  scan_interval: 3600

metadata:
  discogs:
    api_token: "your-discogs-token"
  ai:
    enabled: true
    provider: openai
    api_key: "your-key"
```

## API Overview

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/auth/login` | POST | Authenticate, receive JWT |
| `/auth/refresh` | POST | Refresh JWT |
| `/artists` | GET | List artists (with focus filters) |
| `/artists/{id}` | GET | Artist detail with albums |
| `/albums` | GET | List albums (with focus filters) |
| `/albums/{id}` | GET | Album detail with tracks |
| `/tracks/{id}/stream` | GET | Stream audio file |
| `/tracks/{id}/download` | GET | Download audio file |
| `/playlists` | GET/POST | List/create playlists |
| `/library/scan` | POST | Trigger library scan (admin) |

See [docs/API.md](docs/API.md) for the full API reference.

## iOS Client

The companion iOS app is available separately at [riff-ios](https://github.com/alexmaslar/riff-ios) (private).

## License

[MIT](LICENSE)
