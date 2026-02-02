# Riff API Reference

REST API served by `riff-server`. All responses are JSON. All endpoints except auth require a valid JWT in the `Authorization: Bearer <token>` header.

## Authentication

### POST `/auth/login`

Authenticate and receive a JWT.

**Request:**
```json
{
  "username": "alex",
  "password": "password"
}
```

**Response:**
```json
{
  "token": "eyJ...",
  "user": {
    "id": "uuid",
    "username": "alex",
    "display_name": "Alex",
    "role": "admin"
  }
}
```

### POST `/auth/refresh`

Refresh an expiring JWT.

**Request:** `Authorization: Bearer <token>`

**Response:**
```json
{
  "token": "eyJ..."
}
```

## Artists

### GET `/artists`

List all artists. Supports focus filters.

**Query Parameters:**
- `focus` — Comma-separated filters (e.g., `genre:Jazz,decade:1960s`)
- `search` — Search by name
- `limit` — Results per page (default: 50)
- `offset` — Pagination offset

**Response:**
```json
{
  "artists": [
    {
      "id": "uuid",
      "name": "Miles Davis",
      "album_count": 32,
      "image_url": "/artists/uuid/image"
    }
  ],
  "total": 150
}
```

### GET `/artists/{id}`

Artist detail with albums.

**Response:**
```json
{
  "id": "uuid",
  "name": "Miles Davis",
  "bio": "...",
  "image_url": "/artists/uuid/image",
  "albums": [
    {
      "id": "uuid",
      "title": "Kind of Blue",
      "year": 1959,
      "cover_art_url": "/albums/uuid/art",
      "track_count": 5
    }
  ]
}
```

## Albums

### GET `/albums`

List all albums. Supports focus filters.

**Query Parameters:**
- `focus` — Dynamic filters (see Focus System below)
- `search` — Search by title or artist
- `sort` — `added`, `alpha`, `year` (default: `added`)
- `order` — `asc`, `desc` (default: `desc`)
- `limit`, `offset` — Pagination

**Response:**
```json
{
  "albums": [
    {
      "id": "uuid",
      "title": "Kind of Blue",
      "artist": { "id": "uuid", "name": "Miles Davis" },
      "year": 1959,
      "genre": ["Jazz"],
      "style": ["Modal Jazz"],
      "label": "Columbia",
      "cover_art_url": "/albums/uuid/art",
      "track_count": 5,
      "duration_seconds": 2756,
      "format": "FLAC",
      "sample_rate": 96000,
      "bit_depth": 24,
      "added_at": "2026-01-15T10:30:00Z"
    }
  ],
  "total": 247
}
```

### GET `/albums/{id}`

Album detail with tracks and AI summary.

**Response:**
```json
{
  "id": "uuid",
  "title": "Kind of Blue",
  "artist": { "id": "uuid", "name": "Miles Davis" },
  "year": 1959,
  "genre": ["Jazz"],
  "style": ["Modal Jazz", "Cool Jazz"],
  "label": "Columbia",
  "catalog_number": "CS 8163",
  "cover_art_url": "/albums/uuid/art",
  "ai_summary": "...",
  "tracks": [
    {
      "id": "uuid",
      "title": "So What",
      "track_number": 1,
      "disc_number": 1,
      "duration_seconds": 545,
      "format": "FLAC",
      "sample_rate": 96000,
      "bit_depth": 24,
      "file_size_bytes": 98304000
    }
  ],
  "added_at": "2026-01-15T10:30:00Z"
}
```

## Tracks

### GET `/tracks/{id}/stream`

Stream an audio file. Returns the raw audio data with appropriate `Content-Type` header (`audio/flac`, `audio/mp4`, etc.). Supports `Range` headers for seeking.

### GET `/tracks/{id}/download`

Download an audio file. Same as stream but with `Content-Disposition: attachment` header.

## Playlists

### GET `/playlists`

List playlists for the authenticated user.

**Response:**
```json
{
  "playlists": [
    {
      "id": "uuid",
      "name": "Late Night Jazz",
      "description": "...",
      "track_count": 24,
      "duration_seconds": 5520,
      "is_public": false,
      "created_at": "2026-01-20T18:00:00Z"
    }
  ]
}
```

### POST `/playlists`

Create a playlist.

**Request:**
```json
{
  "name": "Late Night Jazz",
  "description": "Mellow jazz for late evenings"
}
```

### GET `/playlists/{id}`

Playlist detail with tracks.

### PUT `/playlists/{id}`

Update playlist name/description.

### DELETE `/playlists/{id}`

Delete a playlist.

### POST `/playlists/{id}/tracks`

Add tracks to a playlist.

**Request:**
```json
{
  "track_ids": ["uuid1", "uuid2"],
  "position": 0
}
```

### DELETE `/playlists/{id}/tracks`

Remove tracks from a playlist.

**Request:**
```json
{
  "track_ids": ["uuid1"]
}
```

## Users (Admin)

### GET `/users`

List all users. Admin only.

### POST `/users`

Create a user. Admin only.

**Request:**
```json
{
  "username": "newuser",
  "password": "password",
  "display_name": "New User",
  "role": "user"
}
```

## Library Management (Admin)

### POST `/library/scan`

Trigger a library scan. Admin only.

**Response:**
```json
{
  "status": "scanning",
  "message": "Library scan started"
}
```

## Focus Filter System

The `focus` query parameter enables Roon-style dynamic filtering. Multiple filters are comma-separated. Within a category, multiple values use OR logic. Across categories, AND logic applies.

**Syntax:** `dimension:value`

**Supported dimensions:**

| Dimension | Example | Description |
|-----------|---------|-------------|
| `genre` | `genre:Jazz` | Discogs genre |
| `style` | `style:Modal Jazz` | Discogs style |
| `decade` | `decade:1960s` | Release decade |
| `year` | `year:1959` | Exact release year |
| `format` | `format:FLAC` | Audio format |
| `label` | `label:Blue Note` | Record label |
| `added` | `added:last-week` | Time since added |
| `played` | `played:true` | Has been played |
| `favorited` | `favorited:true` | User has favorited |

**Examples:**
```
GET /albums?focus=genre:Jazz,decade:1960s
GET /albums?focus=format:FLAC,added:last-week
GET /artists?focus=genre:Jazz
```

## Data Model

### Core Entities

- **User** — id, username, password_hash, display_name, role, created_at
- **Artist** — id, name, discogs_id, bio, image_url
- **Album** — id, title, artist_id, year, genre, style, label, catalog_number, cover_art_path, ai_summary, added_at
- **Track** — id, album_id, title, track_number, disc_number, duration_seconds, file_path, format, sample_rate, bit_depth, file_size_bytes
- **Playlist** — id, user_id, name, description, is_public, created_at, updated_at
- **PlaylistTrack** — playlist_id, track_id, position
- **PlayHistory** — id, user_id, track_id, played_at, completed
- **Favorite** — user_id, entity_type, entity_id, created_at
