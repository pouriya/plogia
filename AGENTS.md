# plogia

A blog that looks and reads like a Telegram channel. One binary serves a
front end (`--www`) and a directory of Markdown posts (`--posts`), and counts
who read what in SQLite (`--db`). Plain HTTP on `--port`, meant to sit behind a
reverse proxy that terminates TLS.

## ⚠️ Read this before writing any code

**ASK, DO NOT DECIDE.** Every design or behaviour choice — a format, a
dependency, a flag, a table, a UI detail — is the author's. Put the options to
them, recommended first, and wait. Do not pick a default and explain later.

**DO NOT CREATE NEW FILES UNLESS EXPLICITLY TOLD TO.** The backend is
`src/main.rs` and nothing else; the layout below is closed. No `utils.rs`,
`db.rs`, `lib.rs` or any other split on your own initiative. If you think a
split is needed, say so and wait for an answer.

**INLINE EVERYTHING UNLESS YOU KNOW IT WILL BE REUSED.** One caller means no
function: paste the body into the call site. This applies to functions, types,
constants and modules alike. Abstraction is earned by a second caller that
exists today, never by anticipation. When a helper's only other caller
disappears, collapse it back.

**USE `make`. DO NOT CALL `cargo` DIRECTLY.** `cargo build` does not pin
`--target`, `cargo clippy` without `-D warnings` does not fail.
**Before reporting any task finished, `make all` must pass.** Quote its
result; do not claim it passed without running it.

## Make targets

| Target | What it does |
|--------|--------------|
| `make all` | **The gate**: `dev` + `clippy` + `test` + `check-style` |
| `make dev` | Debug build → `build/plogia-<version>-<target>-dev` |
| `make release` | Release build → `build/plogia-<version>-<target>` |
| `make start-dev` | `dev`, then serves `www/` and `posts/` on `PORT` (8080) with `--debug`, database `plogia.db` |
| `make dist` | `build/dist/plogia-<version>-<target>.tar.gz` (binary + `www/`) and its `.sha256` |
| `make docker` | Alpine image `plogia:<version>` and `plogia:latest`, `www/` and `posts/` baked in |
| `make clippy` | `cargo clippy --all-targets --no-deps -- -D warnings` |
| `make check-style` | `cargo fmt --check` |
| `make fmt` | Rewrites formatting in place |
| `make lint` | `clippy` + `check-style`, no build |
| `make test` | `cargo test --target …` |
| `make clean` / `dist-clean` | Drop `target/` / also `build/` |

## Layout

```
src/main.rs     The whole backend: settings consts, CLI, schema, routes, handlers
www/            The front end, served as-is from --www
  index.html    Template for /, /index.html and /p/<n>; {{name}} filled by the server
  app.js        Handshake, feed drawing, paging, view reporting, message menu, channel info
  style.css     Telegram light / Night, by prefers-color-scheme
  plogia.jpg    Channel avatar and default link-preview image
  favicon.ico   16, 32, 48 px from plogia.jpg
  apple-touch-icon.png   180 px from plogia.jpg
  fonts/        Vazirmatn Regular, Medium, Bold (woff2) and its OFL license
posts/          The channel's posts, one folder each
Makefile        Every build/check entry point. Use it, not cargo.
Dockerfile      Pinned rust-alpine builder, pinned alpine runtime
```

## Settings

No configuration file and no environment variables. What the channel *is*
lives as `const`s at the top of `src/main.rs`: `CHANNEL_TITLE` (`Π`, Greek
capital pi), `CHANNEL_DESCRIPTION`, `CHANNEL_AVATAR`, `TIMEZONE` (`+03:30`, the
zone post dates are written in), `PAGE_SIZE` (20), `TEXT_LIMIT`,
`ROBOT_MARKERS`. What differs per machine comes from the command
line, and every flag is mandatory with no default:

```
plogia --www DIR --posts DIR --db FILE --port PORT [--debug | --trace | --quiet]
```

It binds `0.0.0.0:<port>`, so it works inside a container; on a host, keep the
port firewalled so only the proxy reaches it.

## Posts

```
posts/
  12/
    post.md     ---\ndate: 2026-10-19 14:30\n---\nMarkdown body
    cat.jpg     linked from post.md as ![](cat.jpg), served at /posts/12/cat.jpg
```

- The folder name is the post number: its id, its order and its permalink
  (`/p/12`). Numbers need not be contiguous.
- `date` is `YYYY-MM-DD HH:MM`, the author's wall clock, no zone.
- **A single newline is a line break**, like typing in Telegram; a blank line is
  a new paragraph.
- Relative links and images resolve into the post's own folder.
- `post.md` itself, dotfiles and anything outside a numbered folder are never
  served.
- **Nothing is kept in memory.** Every page load and every `GET /api/posts`
  lists the directory and reads the posts, so a post is live as soon as its
  folder exists. A post that fails to parse is skipped with an `error` log
  naming the file; it never takes the channel down.

## HTTP

| Route | What |
|-------|------|
| `GET /`, `/index.html`, `/p/{n}` | `www/index.html` with link-preview tags filled; unknown `n` is a 404 that still shows the channel |
| `GET /posts/{n}/{file}` | A post's media |
| `POST /api/visit` | The handshake, once per page open: `{device?, path, referrer}` → `{device, subscribers}` |
| `GET /api/posts?before=n\|after=n\|around=n` | One page of posts, oldest first, with `older`/`newer` flags, view totals and the channel's `total` post count |
| `POST /api/views` | `{device, posts}` → `{views}`, the new totals |
| anything else | A file from `--www` |

Behind the proxy, the client address is the right-most entry of `Forwarded:
for=`, then `X-Forwarded-For`, then `X-Real-IP`, then the socket. The
right-most entry is the one our own proxy appended; a client can forge
anything to its left. Link previews take scheme and host from `Forwarded`,
`X-Forwarded-Proto`/`-Host`, then `Host`.

**Link previews** — what Telegram, WhatsApp and X show when someone pastes our
link. Their bots are counted as robots but still get the page and its tags.
`/` is `og:type=website` with `CHANNEL_TITLE` and `CHANNEL_DESCRIPTION`. `/p/<n>`
is `og:type=article`: the title is the post's first sentence (up to the first
`.` `!` `?` `؟` followed by a space, or the first line break; 100 characters at
most), the description is the 200 characters after it, and
`article:published_time` is the date in `TIMEZONE`. A post with no text is
titled `Π · <n>`. Also `og:locale=fa_IR`, `rel=canonical`, `twitter:card=summary`.
No `og:image`, on purpose: previews are text only.

Everything answers `Cache-Control: no-cache` unless the handler set its own:
the browser revalidates and gets a 304 for an unchanged file.

## Devices, visits, views, robots

```sql
device (id BLOB PRIMARY KEY, created_at)          -- ULID, one per browser
visit  (id, device, at, ip, user_agent, referrer, language, path)   -- every page open
view   (id, post, device, at)                     -- every view, for views-over-time graphs
robot  (day, kind, dropped, PRIMARY KEY (day, kind))  -- refused robots, per UTC day
```

- **The device id is a ULID** issued by `POST /api/visit` and kept in the
  browser's `localStorage`. An id the database does not know gets a new one.
  48 bits of milliseconds then 80 random bits from `getrandom`, Crockford
  base32, hand-written after borhan's, no monotonic factory.
- **A view is a post at least half on screen for two continuous seconds**, in
  a visible tab, sent at most once per post per page load. Fast scrolling
  counts nothing; leaving and coming back, or refreshing, counts again. A
  post's view count is its number of `view` rows.
- **Subscribers** in the header is the number of `device` rows.
- **Robots** are refused from devices, visits and views and only counted: a
  User-Agent not starting with `Mozilla/`, or containing one of
  `ROBOT_MARKERS`, is a robot; the front end also sends nothing when
  `navigator.webdriver` is set. Times are Unix milliseconds.

## Front end

Client-drawn from the JSON API; the server only fills the template. Newest
post at the bottom, the page opens there, older posts load as the reader
scrolls up. Text direction is per block (`dir="auto"`), so Persian and English
lines each read the right way. Vazirmatn ships in `www/fonts`, no CDN.

- **Message menu**: right click, or a long press on touch, opens Copy Text,
  Copy Link (`/p/<n>`) and Forward. A round forward button sits beside each
  bubble, on hover with a mouse and always on touch. Forward shares the link
  alone through the system share sheet (`navigator.share`); where there is
  none, it copies the link. Every copy shows a toast.
- **Channel info**: no intro card in the feed. The header, or a post's avatar,
  opens a Telegram-style panel — beside the feed from 1000px, over it below,
  the whole screen at phone width — with the avatar, title, subscribers, a
  Share button (the channel link, as Forward), the description, the site link
  (tap copies) and the post count. Opening it is a history entry: back, ✕ or
  Escape closes it.
- **Language**: English or Persian, English by default. A saved choice in
  `localStorage` (`plogia.language`) wins; without one, the first English or
  Persian entry in `navigator.languages` (browser, then system) does. The
  channel info has a row that toggles it, in place, no reload. Persian turns
  the whole layout right to left, keeps Latin digits, and dates day pills and
  time tooltips in the solar Hijri calendar (`Intl`, `fa-IR-u-ca-persian`).
  Every UI string lives in `STRINGS` in `app.js`; static ones in `index.html`
  carry `data-text`/`data-label` with their key. Posts themselves are never
  translated, and keep their own per-block direction.
- **Profile photo**: tapping the big avatar in the channel info opens it full
  screen, Telegram's viewer: a top bar with avatar, title and ✕. Another
  history entry: back, ✕, Escape or a tap beside the photo returns to the info.

## Docker

Built by `make docker` from base images pinned to exact versions —
`DOCKER_RUST_IMAGE` (`rust:1.98.1-alpine3.24`) and `DOCKER_ALPINE_IMAGE`
(`alpine:3.24.2`), defaults in both the Makefile and the Dockerfile — so a
build reuses what is cached and never pulls a tag that moved. Docker Hub is not
reachable from every build machine; `DOCKER_REGISTRY` prefixes both base images
when a mirror is needed. The image is pushed as
`registry.hamdocker.ir/plogia/plogia:<version>` and `:latest`
(`make docker DOCKER_IMAGE=registry.hamdocker.ir/plogia/plogia`). `www/` and
`posts/` are baked in, so publishing a post is building and pushing a new
image. The database is the only state: `VOLUME /var/lib/plogia`.

```
docker run -d -p 8080:8080 -v plogia:/var/lib/plogia plogia:latest
```

## Code style conventions

- **Plain `for` loops over iterator method chains.**
- **`match` over combinators for `Result`**, **`if let`/`match` over
  combinators for `Option`.** Where clippy insists otherwise, clippy wins.
- **Don't extract single-use helpers.** Same rule as at the top.
- **`anyhow` at the binary boundary**: `fn main() -> anyhow::Result<()>`, `?`
  plus `.context(...)`. Never convert errors to `String` to propagate them.

## Lint & style conventions

- **No `#[allow(...)]` anywhere** — fix the root cause.
- **`make clippy` is the gate** — `-D warnings`.

## Testing

No `#[cfg(test)]` blocks in `src/` — ever. Tests live in `tests/` and drive the
built binary over HTTP; there are none yet.

## Logging conventions

`tracing` only, JSON to stderr, installed in `main`. **Default level is
`INFO`**, because this is a server and its journal wants the access log.

| Flag | Level | Extra fields |
|------|-------|--------------|
| `--quiet` | `OFF` | — |
| *(none)* | `INFO` | — |
| `--debug` | `DEBUG` | target |
| `--trace` | `TRACE` | target, file, line |

Every request logs one event: `Handled request` (2xx/3xx, `info`), `Rejected
request` (4xx, `warn`) or `Failed request` (5xx, `error`), with
`http.request.method`, `http.route`, `url.path`, `url.query`,
`http.response.status_code` and `total_ms`.

Every log call starts with a `msg` field whose value begins with a capital
letter; structured fields follow. `?` for `Debug`, `%` for `Display`.

```rust
tracing::error!(msg = "Skipped unreadable post", post = id, error = %format!("{error:#}"));
```

| Level | When to use |
|-------|-------------|
| `info` | Important success event (listening, a request handled) |
| `warn` | Intentionally ignored error, or a fallback being taken |
| `error` | Failure the server recovers from; one it does not is returned from `main` |
| `debug` | Before attempting something important, with its inputs |
| `trace` | After a low-level operation, with what it produced |
