# Local control API

An opt-in HTTP API on loopback so other apps on the same computer (Raycast, a Stream Deck plugin,
a shell script) can control Limusic. It is **off by default**.

Turn it on in **Settings → Data → Local API**. Limusic then listens on `127.0.0.1:<port>` (default
`26538`) and shows an access token to paste into the client. Changes apply immediately; no restart.

## Security

- Bound to `127.0.0.1` only. Nothing on your network can reach it.
- Every request needs `Authorization: Bearer <token>`. The token is 256 random bits, stored in
  Limusic's database; **Regenerate** invalidates it for every client at once.
- Requests with an `Origin` header are refused (`403 FORBIDDEN_ORIGIN`), and so is any `Host`
  other than `127.0.0.1:<port>` or `localhost:<port>` (`403 FORBIDDEN_HOST`). Together these stop
  web pages in your browser from using the API through CSRF or DNS rebinding, even with the token.

## Conventions

- Base path `/api/v1`. JSON in and out. Times are milliseconds.
- Success without a payload answers `{"ok": true}`.
- Errors answer `{"error": {"code": "...", "message": "..."}}` with a matching status:

| Status | Code | When |
|---|---|---|
| 400 | `BAD_REQUEST` | Missing/invalid body or query |
| 401 | `UNAUTHORIZED` | Missing or wrong token |
| 403 | `FORBIDDEN_ORIGIN` / `FORBIDDEN_HOST` | Browser or rebinding request |
| 404 | `NOT_FOUND` | Unknown route, song, or queue index |
| 409 | `NOT_LOGGED_IN` | Needs a signed-in account (liking) |
| 502 | `UPSTREAM_ERROR` | YouTube or the player refused |
| 503 | `DISABLED` | The API was just switched off |
| 504 | `TIMEOUT` | No answer within 15 s |

## Objects

```jsonc
// Track
{ "id": "videoId", "title": "…", "artists": "…", "album": "…" | null, "albumId": "…" | null,
  "durationMs": 225000 | null, "thumbnailUrl": "…" | null, "liked": true | false | null }

// Card (album / artist / playlist)
{ "type": "album" | "artist" | "playlist" | "song", "id": "browseId", "title": "…",
  "subtitle": "…" | null, "thumbnailUrl": "…" | null }
```

## Endpoints

| Method | Path | Body / query | Answer |
|---|---|---|---|
| GET | `/status` | | `{version, apiVersion}` |
| GET | `/player` | | `{track: Track \| null, positionMs, isPlaying, volume, shuffle, repeat}` |
| POST | `/player/play` · `/pause` · `/toggle` · `/next` · `/previous` | | ok |
| POST | `/player/seek` | `{positionMs}` | ok |
| POST | `/player/volume` | `{volume: 0-100}` | ok |
| POST | `/player/shuffle` | `{enabled?}` (omit to toggle) | ok |
| POST | `/player/repeat` | `{mode?: "off"\|"all"\|"one"}` (omit to cycle) | ok |
| GET | `/search` | `?q=…&type=songs\|albums\|artists\|playlists` | `{type, results: (Track + type:"song")[] \| Card[]}` |
| POST | `/play` | `{type: "song"\|"album"\|"playlist"\|"artist", id, shuffle?, startIndex?}` | ok |
| GET | `/queue` | | `{currentIndex, tracks: Track[]}` |
| POST | `/queue` | `{videoId, position?: "next"\|"end"}` (default `end`) | ok |
| POST | `/queue/jump` | `{index}` | ok |
| DELETE | `/queue/{index}` | | ok |
| GET | `/library/playlists` | | `{playlists: Card[]}` |
| GET | `/library/playlists/{id}` | | `{id, title, subtitle, thumbnailUrl, tracks: Track[]}` |
| POST | `/track/like` | `{videoId?, liked}` (defaults to the playing track) | ok |

`/play` with `type: "artist"` starts the artist's radio, like the artist page's play button.

## Example

```sh
TOKEN=…   # from Settings → Data → Local API
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:26538/api/v1/player
curl -s -X POST -H "Authorization: Bearer $TOKEN" http://127.0.0.1:26538/api/v1/player/toggle
curl -s -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:26538/api/v1/search?q=daft+punk&type=albums"
```
