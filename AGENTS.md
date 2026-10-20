# plogia

A blog that looks and reads like a Telegram channel. One binary serves a
front end (`--www`) and a directory of Markdown posts (`--posts`), counts who
read what in SQLite (`--db`), and emails each new post to its subscribers,
straight to their mail servers. Plain HTTP on `--port`, meant to sit behind a
reverse proxy that terminates TLS; `--url` is the address readers use.

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
| `make start-dev` | `dev`, then serves `www/` and `posts/` on `PORT` (8080) with `--debug`, database `plogia.db`, `--url` from `URL` (`http://localhost:$PORT`) |
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
src/main.rs     The whole backend: settings consts, CLI, schema, DKIM key, email sender, routes, handlers
www/            The front end, served as-is from --www
  index.html    Template for /, /index.html and /p/<n>; {{name}} filled by the server
  app.js        Handshake, feed drawing, paging, view reporting, message menu, channel info, Join
  style.css     Telegram light / Night, by prefers-color-scheme
  plogia.jpg    Channel avatar and default link-preview image
  favicon.ico   16, 32, 48 px from plogia.jpg
  apple-touch-icon.png   180 px from plogia.jpg
  fonts/        Vazirmatn Regular, Medium, Bold (woff2) and its OFL license
posts/          The channel's posts, one folder each
Makefile        Every build/check entry point. Use it, not cargo.
README.md       For people running their own: run, write a post, email setup
LICENSE         MIT
Dockerfile      Pinned rust-alpine builder, pinned alpine runtime
```

## Settings

No configuration file and no environment variables. What the channel *is*
lives as `const`s at the top of `src/main.rs`: `CHANNEL_TITLE` (`Π`, Greek
capital pi), `CHANNEL_DESCRIPTION`, `CHANNEL_AVATAR`, `CHANNEL_TELEGRAM`
(`https://t.me/plogia`), `CHANNEL_GITHUB` (`https://github.com/pouriya/plogia`),
`EMAIL_TITLE` (the subject prefix, `CHANNEL_DESCRIPTION` for now),
`EMAIL_USER` (`noreply`), `DKIM_SELECTOR` (`plogia`), `TIMEZONE` (`+03:30`, the
zone post dates are written in), `PAGE_SIZE` (20), `TEXT_LIMIT`,
`ROBOT_MARKERS`. What differs per machine comes from the command
line, and every flag is mandatory with no default:

```
plogia --www DIR --posts DIR --db FILE --port PORT --url URL [--debug | --trace | --quiet]
```

`--url` (`https://plogia.ir`) is where readers reach the channel. Link
previews, the RSS feed and emails link there, and its host is the email domain:
`noreply@<host>`, the EHLO name and the DKIM `d=`.

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
| `POST /api/visit` | The handshake, once per page open: `{device?, path, referrer}` → `{device, subscribers}`, `subscribers` being the email subscribers |
| `GET /api/posts?before=n\|after=n\|around=n` | One page of posts, oldest first, with `older`/`newer` flags, view totals and the channel's `total` post count |
| `POST /api/views` | `{device, posts}` → `{views}`, the new totals |
| `GET /api/captcha` | `{id, image}`, a `data:image/png` URL; `429` + `Retry-After: 1` over the limit |
| `POST /api/subscribe` | `{email, captcha, answer}` → `204` (new or already subscribed, alike), or `400 {error: "email" \| "captcha"}` |
| `GET /rss.xml` | RSS 2.0, the newest `PAGE_SIZE` posts, whole HTML with absolute links |
| anything else | A file from `--www` |

Behind the proxy, the client address is the right-most entry of `Forwarded:
for=`, then `X-Forwarded-For`, then `X-Real-IP`, then the socket. The
right-most entry is the one our own proxy appended; a client can forge
anything to its left. Link previews take their absolute URLs from `--url`,
never from request headers.

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
subscriber (id, email UNIQUE COLLATE NOCASE, ip, at)  -- every email from the Join panel
delivery   (post, subscriber, status, attempts, next_at, error, at,
            PRIMARY KEY (post, subscriber))           -- what post went to whom
key    (name PRIMARY KEY, value)                      -- `dkim`: the signing key, PEM
```

- **The device id is a ULID** issued by `POST /api/visit` and kept in the
  browser's `localStorage`. An id the database does not know gets a new one.
  48 bits of milliseconds then 80 random bits from `getrandom`, Crockford
  base32, hand-written after borhan's, no monotonic factory.
- **A view is a post at least half on screen for two continuous seconds**, in
  a visible tab, sent at most once per post per page load. Fast scrolling
  counts nothing; leaving and coming back, or refreshing, counts again. A
  post's view count is its number of `view` rows.
- **Subscribers** in the header, the channel info and the Join panel is the
  number of `subscriber` rows: emails, not devices.
- **Robots** are refused from devices, visits, views, captchas and subscribing
  and only counted: a
  User-Agent not starting with `Mozilla/`, or containing one of
  `ROBOT_MARKERS`, is a robot; the front end also sends nothing when
  `navigator.webdriver` is set. Times are Unix milliseconds.

## Subscribers and post emails

- **Subscribing** takes an email and a captcha, nothing else: no confirmation,
  no removal yet. The email is kept as first typed, compared without case; the
  same one again answers the same `204` and changes nothing. The client
  address is kept beside it.
- **Captchas** are the `captcha` crate's Easy preset (Lucy), held in memory
  only: at most 1000, the oldest pushed out past that, each good for 10
  minutes and one try, answered without case. One per client address per
  second, and at most 100 addresses in any second; the rest get `429`. The
  email is checked before the captcha, so a typo does not cost one.
- **Delivery rows**: when someone subscribes, every post out at that moment is
  marked `skipped` for them, so they get only what comes after, and their
  welcome is a `pending` row for post `-1` (post numbers are never negative).
  At every start the server gives each subscriber a `pending` row for every
  parsable post they have none for. One sender sends the due rows one at a
  time, oldest first; with none due it waits until a new subscriber or the
  next retry wakes it. **New posts are emailed on the next start**, not when
  their folder appears; welcomes go at once.
- **noreply addresses**: an email whose user part contains `noreply` or
  `no-reply`, in any case, is accepted and counted like any other, but sent
  nothing: its welcome and every post get a `skipped` row with error
  `noreply address`, each logged at `warn`. The sender checks again before
  every send, so rows owed from before the rule are skipped too.
- **The welcome**: only to an email that is new; subscribing a known one again
  sends nothing, so the form cannot flood an inbox. Subject `EMAIL_TITLE · عضو
  شدید / You're subscribed`; Persian first, then English: a greeting, "به Π
  پیوستید. هر پستی که از این پس منتشر شود، به همین نشانی برایتان فرستاده
  می‌شود." / "You're subscribed to Π. Every new post will come to this
  address.", and named links to the channel, Telegram and RSS (spelled out in
  the plain-text part).
- **No relay**: each email goes to the recipient domain's MX hosts (by
  preference; the domain itself without MX records; refused on a null MX) on
  port 25, with opportunistic STARTTLS that accepts any certificate, as mail
  servers do among themselves. A host that cannot be reached falls through to
  the next.
- **Retries**: only a 4xx answer is tried again, after 5 minutes, 30 minutes
  and 2 hours; the fourth 4xx, a 5xx, or any other error marks the row
  `failed` with its `error`, never tried again. `sent` and `failed` both count
  as delivered.
- **The email**: from `Π <noreply@<host of --url>>`, subject `EMAIL_TITLE ·
  <first sentence>` (`EMAIL_TITLE · <n>` without one), the post's HTML with
  absolute links and a plain-text part, both ending in a "View in channel" link
  to `/p/<n>`, with a `Message-ID` and DKIM-signed (RSA-SHA256).
- **DKIM**: an RSA 2048 key is generated into `key` on first start and kept;
  every start logs the TXT record to publish at
  `plogia._domainkey.<host>`. Mail only lands with that record, SPF and DMARC
  for the host, a PTR for the server's IP matching it, and outbound port 25
  open where the server runs.

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
  Share button (the channel link, as Forward), then the description and the
  post count, then one line of tiles — logo and title, never the address —
  for Website (tap copies the site link), Telegram, RSS (tap copies the feed
  URL) and GitHub, then the language switch. Telegram and GitHub open in a new
  tab. Opening it is a history entry: back, ✕ or Escape closes it.
- **Join**: a JOIN CHANNEL bar under the feed, where Telegram puts it for a
  non-member, opens the Join panel (same place and history rules as the
  channel info): avatar, title, subscribers, then the email form with its
  captcha (↻ for a new one), then Telegram and RSS tiles. A successful subscribe
  shows a toast ("You're in. Check your inbox." / «به کانال پیوستید؛ ایمیلتان
  را ببینید.»), closes the panel and sets `plogia.joined` in `localStorage`;
  the bar is never shown again in that browser.
- **Language**: English by default; Persian only when the reader turns on the
  فارسی switch in the channel info, kept in `localStorage` (`plogia.language`).
  The browser's languages are not consulted. It switches in place, no reload.
  Subscribers are "عضو" in Persian, to match پیوستن and عضویت. Persian turns
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
when a mirror is needed. `DOCKER_IMAGE` names the image for a registry to push
to (`make docker DOCKER_IMAGE=<registry>/plogia`). `www/` and
`posts/` are baked in, so publishing a post is building and pushing a new
image. The database is the only state: `VOLUME /var/lib/plogia`.
The runtime image carries the `sqlite3` shell for debugging:
`docker exec -it <container> sqlite3 /var/lib/plogia/plogia.db`.

```
docker run -d -p 8080:8080 -v plogia:/var/lib/plogia plogia:latest --url https://plogia.ir
```

`--url` is the one flag the image leaves to you: it goes after the image name.

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

Every email logs `Sent email` (`info`, with the `host` that took it),
`Deferred email` (`warn`, a 4xx, with `minutes` until the retry) or `Could not
send email` (`error`), each with `kind` (`welcome` or `post`), `post` (`-1` for
a welcome), `email` and the receiving server's own reply in `error`. A mail
host that cannot be reached logs `Could not reach mail host, trying the next`
(`warn`). The same error stays in `delivery.error`.

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
