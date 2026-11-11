# PhantomQA API

A read-only JSON API over the test reports, for dashboards and scripts. The machine-readable version is [`openapi.json`](openapi.json), also served at `/openapi.json`.

## Start it

```sh
phantomqa token add dashboard                 # prints a token once; store it in your dashboard
phantomqa serve                               # http://127.0.0.1:7070
phantomqa serve --listen 0.0.0.0:7070         # reachable from other machines (plain HTTP)
```

- `serve` refuses to start while no token exists.
- It listens on this machine only, unless you pass `--listen`. For access from other machines, put an HTTPS proxy in front, so tokens never travel unencrypted.
- `PHANTOMQA_LISTEN` sets the address too. `--reports` and `--tokens` work as for the other commands.
- One line per request goes to stderr: method, path, status and the token's name (never the token).

## Authentication

Every `/v1` route needs a token:

```
Authorization: Bearer pqa_...
```

A missing, malformed, unknown, expired or revoked token gets the same answer, on purpose:

```
401 Unauthorized
WWW-Authenticate: Bearer realm="phantomqa"
{"error": {"code": "unauthorized", "message": "send a valid token: Authorization: Bearer pqa_..."}}
```

Revoking or rotating a token works on the next request. Without a token, unknown `/v1` paths also return 401, so the routes stay hidden.

## Endpoints

| Method | Path | Token | Returns |
|---|---|---|---|
| GET | `/health` | no | `{"ok": true, "version": "0.1.0"}` |
| GET | `/openapi.json` | no | the OpenAPI 3.1 description |
| GET | `/v1/runs?limit=50` | yes | runs, newest first |
| GET | `/v1/runs/{run}` | yes | one run: status, device, bugs, files |
| GET | `/v1/runs/{run}/files/{name}` | yes | a file of the run |
| GET | `/v1/runs/{run}/files/video/{name}` | yes | a raw video part |

HEAD works wherever GET does. Other methods get `405` with `Allow: GET, HEAD`.

### List runs

```sh
curl -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:7070/v1/runs?limit=2"
```

```json
{"runs": [
  {"run": 4, "status": "running", "bugs": 1},
  {"run": 3, "status": "finished", "bugs": 2}
]}
```

`limit` is 1 to 1000 (default 50); anything else is a `400`.

### One run

```sh
curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:7070/v1/runs/3
```

```json
{
  "run": 3,
  "status": "finished",
  "device": "Pixel 7, Android 14 (API 34)",
  "bugs": [
    {
      "bug": 1,
      "title": "Crash in com.example: java.lang.NullPointerException",
      "severity": "critical",
      "app": "com.example",
      "files": {"screenshot": "bug-01.png", "video": "bug-01.mp4", "log": "bug-01.log"}
    }
  ],
  "session_video": "session.mp4",
  "files": ["bug-01.log", "bug-01.mp4", "bug-01.png", "report.md", "run.json", "session.mp4"]
}
```

| Status | Meaning |
|---|---|
| `running` | the test session is active |
| `finished` | the session ended normally |
| `interrupted` | the session stopped without finishing: killed or crashed |
| `unknown` | the run has no `run.json` (made by an older version) |
| `broken` | `run.json` cannot be read; `error` says why |

Runs started by `phantomqa run` also have `test` (the test file's settings: `file`, `app`, `goal`, `avoid`, `model`, `max_minutes`, and `apk` and `login` when set, never the password) and `agent`:

```json
"agent": {"name": "claude", "model": "haiku", "ended": "done", "summary": "I opened ...", "cost_usd": 0.003, "turns": 12, "error": null}
```

`ended` is `done`, `time limit` or `error`; `error` says why. `agent` appears when the run is over.

A bug's `files` lists only the files that exist. When no single video clip could be made (no ffmpeg), the bug has `video_parts`, such as `["video/part-003.mp4", "video/part-004.mp4"]`, instead of `video`.

### Files

```sh
curl -H "Authorization: Bearer $TOKEN" -O http://127.0.0.1:7070/v1/runs/3/files/bug-01.png
curl -H "Authorization: Bearer $TOKEN" -r 0-1048575 http://127.0.0.1:7070/v1/runs/3/files/session.mp4
```

- Content types: `report.md` is `text/markdown`, `.png` is `image/png`, `.mp4` is `video/mp4`, `.log` is `text/plain`, `.json` is `application/json`.
- Every file response has `Content-Length` and `Accept-Ranges: bytes`. A video player can seek with `Range`.
- `Range` takes one range: `bytes=0-1023`, `bytes=1024-` or `bytes=-500`. The answer is `206` with `Content-Range`. A range that starts past the end gets `416` with `Content-Range: bytes */SIZE`. Several ranges, or a value that can't be read, return the whole file.
- Names may use letters, digits, `.`, `_` and `-`, and must not start with `.`. Hidden files, other folders and links are never served, so paths like `../` cannot leave the run folder.

## Errors

All errors are JSON:

```json
{"error": {"code": "not_found", "message": "no such run"}}
```

| Status | `code` |
|---|---|
| 400 | `bad_request` |
| 401 | `unauthorized` |
| 404 | `not_found` |
| 405 | `method_not_allowed` |
| 414 | `uri_too_long` (URLs over 2048 characters) |
| 416 | `range_not_satisfiable` |
