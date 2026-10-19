//! plogia: a blog that looks and reads like a Telegram channel.
//!
//! Every request reads what it needs from disk — `--www` for the front end,
//! `--posts` for the posts — and nothing about either is kept in memory, so
//! publishing is writing a file. SQLite (`--db`) holds who came and what they
//! read: one row per device, per visit and per view, and a per-day counter of
//! robots turned away.

use std::{
    collections::HashMap,
    fmt,
    fmt::Write,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path as FilePath, PathBuf},
    sync::{Arc, Mutex},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, bail};
use axum::{
    Json, Router,
    body::Body,
    extract::{ConnectInfo, MatchedPath, Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use clap::Parser;
use pulldown_cmark::{CowStr, Event, Options, Tag, TagEnd};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde::{Deserialize, Serialize};
use tower_http::{
    services::{ServeDir, ServeFile},
    set_header::SetResponseHeaderLayer,
};
use tracing_subscriber::{filter::LevelFilter, fmt as subscriber};

/// The channel's name in the header, on every post and in link previews.
/// Greek capital pi, U+03A0.
const CHANNEL_TITLE: &str = "Π";

/// Under the avatar on the card at the very top of the channel, and the
/// description of a link to the channel itself.
const CHANNEL_DESCRIPTION: &str = "گفته‌های پوریا";

/// A path under `--www`: the avatar in the header and beside every post, and
/// the link-preview image of any page that has no image of its own.
const CHANNEL_AVATAR: &str = "/plogia.jpg";

/// Posts per `GET /api/posts`: the first screen, and every scroll-up after it.
const PAGE_SIZE: usize = 20;

/// Longest header or client-sent string kept in a `visit` row, in characters.
/// A User-Agent is rarely over 300; this only stops a hostile one filling the
/// disk.
const TEXT_LIMIT: usize = 512;

/// Lowercase User-Agent fragments that mark a robot. Cheap and incomplete on
/// purpose: a browser-faking crawler gets through this, but almost none of
/// those run the JavaScript that sends views, and none sit on a post for the
/// two seconds a view needs.
const ROBOT_MARKERS: [&str; 22] = [
    "bot",
    "crawl",
    "spider",
    "slurp",
    "headless",
    "preview",
    "scrape",
    "fetch",
    "curl",
    "wget",
    "python",
    "java/",
    "go-http",
    "okhttp",
    "axios",
    "libwww",
    "httpclient",
    "lighthouse",
    "phantomjs",
    "facebookexternalhit",
    "whatsapp",
    "monitor",
];

/// Serve a Telegram-style channel from a directory of posts.
///
/// Every page load reads `--www/index.html` and the posts it shows from disk,
/// so a new post is live as soon as its folder exists. Meant to sit behind a
/// reverse proxy that terminates TLS; the client address is read from
/// `Forwarded`, `X-Forwarded-For` or `X-Real-IP`, in that order.
#[derive(Debug, Parser)]
#[command(version, author)]
struct CommandLine {
    /// The front end: `index.html` and every file it links to, served as-is.
    #[arg(long, value_name = "DIRECTORY")]
    www: PathBuf,

    /// One folder per post, named by its number: `<n>/post.md` and the files
    /// it links to, which are served under `/posts/<n>/`.
    #[arg(long, value_name = "DIRECTORY")]
    posts: PathBuf,

    /// SQLite database for devices, visits, views and robot counters. Created
    /// if missing.
    #[arg(long, value_name = "FILE")]
    db: PathBuf,

    /// TCP port to listen on, on every interface.
    #[arg(long)]
    port: u16,

    /// Enable trace level logging (shows target and location).
    #[arg(long)]
    trace: bool,

    /// Enable debug level logging (shows target).
    #[arg(long)]
    debug: bool,

    /// Disable all logging.
    #[arg(long)]
    quiet: bool,
}

struct App {
    www: PathBuf,
    posts: PathBuf,
    database: Mutex<Connection>,
}

/// A post as `GET /api/posts` returns it.
#[derive(Serialize)]
struct Post {
    id: u32,
    /// `YYYY-MM-DDTHH:MM`, the author's wall clock, no zone.
    date: String,
    html: String,
    views: u64,
    /// Plain-text start of the post, for the link preview of `/p/<n>`.
    #[serde(skip)]
    excerpt: String,
    /// First image in the post, for the link preview of `/p/<n>`.
    #[serde(skip)]
    image: Option<String>,
}

/// 48 bits of Unix milliseconds then 80 random bits, big-endian, written as 26
/// Crockford base32 characters — borhan's ULID without its error enum. No
/// monotonic factory: two made in one millisecond sort arbitrarily.
struct Ulid([u8; 16]);

/// Crockford's base32: no `I`, `L`, `O` or `U`.
const ULID_ALPHABET: [u8; 32] = *b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

impl Ulid {
    fn new() -> anyhow::Result<Self> {
        let milliseconds = now() as u64;
        let mut bytes = [0u8; 16];
        bytes[..6].copy_from_slice(&milliseconds.to_be_bytes()[2..]);
        if let Err(error) = getrandom::fill(&mut bytes[6..]) {
            bail!("Could not read random bytes from the operating system: {error}");
        }
        Ok(Self(bytes))
    }

    /// Case-insensitive, since a ULID that went through localStorage, a URL or
    /// a copy-paste may come back lowercased. `None` for anything that is not
    /// exactly 26 Crockford digits starting at or below `7`.
    fn parse(text: &str) -> Option<Self> {
        if text.len() != 26 {
            return None;
        }
        let mut value: u128 = 0;
        for (index, character) in text.bytes().enumerate() {
            let upper = character.to_ascii_uppercase();
            let mut digit = None;
            for (position, letter) in ULID_ALPHABET.iter().enumerate() {
                if *letter == upper {
                    digit = Some(position as u128);
                }
            }
            let digit = digit?;
            // 26 characters carry 130 bits; the first has only 3 to spend.
            if index == 0 && digit > 0b111 {
                return None;
            }
            value = (value << 5) | digit;
        }
        Some(Self(value.to_be_bytes()))
    }
}

impl fmt::Display for Ulid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = u128::from_be_bytes(self.0);
        for index in 0..26 {
            let shift = 5 * (25 - index);
            formatter.write_char(char::from(
                ULID_ALPHABET[((value >> shift) & 0b11111) as usize],
            ))?;
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let settings = CommandLine::parse();

    let level = if settings.quiet {
        LevelFilter::OFF
    } else if settings.trace {
        LevelFilter::TRACE
    } else if settings.debug {
        LevelFilter::DEBUG
    } else {
        LevelFilter::INFO
    };
    subscriber::Subscriber::builder()
        .with_max_level(level)
        .json()
        .flatten_event(true)
        .with_current_span(false)
        .with_span_list(true)
        .log_internal_errors(true)
        .with_level(true)
        .with_file(level == LevelFilter::TRACE)
        .with_line_number(level == LevelFilter::TRACE)
        .with_target(matches!(level, LevelFilter::DEBUG | LevelFilter::TRACE))
        .with_thread_ids(false)
        .with_thread_names(false)
        .with_writer(std::io::stderr)
        .init();

    // Checked once here so that a typo'd path fails at startup, naming the
    // flag, rather than as a 500 on the first visitor.
    if !settings.www.join("index.html").is_file() {
        bail!(
            "--www {} has no index.html; it must be the front-end directory",
            settings.www.display()
        );
    }
    if !settings.posts.is_dir() {
        bail!("--posts {} is not a directory", settings.posts.display());
    }

    let connection = Connection::open(&settings.db)
        .with_context(|| format!("Could not open --db {}", settings.db.display()))?;
    connection.busy_timeout(std::time::Duration::from_secs(30))?;
    connection
        .execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;

             -- One row per browser, made by the first POST /api/visit from it.
             -- The id is a ULID, so its first 6 bytes are created_at again.
             CREATE TABLE IF NOT EXISTS device (
                 id BLOB PRIMARY KEY,
                 created_at INTEGER NOT NULL
             ) WITHOUT ROWID;

             -- One row per page open, the device's first included.
             CREATE TABLE IF NOT EXISTS visit (
                 id INTEGER PRIMARY KEY,
                 device BLOB NOT NULL REFERENCES device (id),
                 at INTEGER NOT NULL,
                 ip TEXT NOT NULL,
                 user_agent TEXT NOT NULL,
                 referrer TEXT NOT NULL,
                 language TEXT NOT NULL,
                 path TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS visit_device ON visit (device, at);

             -- One row per post seen for two seconds, at most once per page
             -- load. A post's view count is its number of rows.
             CREATE TABLE IF NOT EXISTS view (
                 id INTEGER PRIMARY KEY,
                 post INTEGER NOT NULL,
                 device BLOB NOT NULL REFERENCES device (id),
                 at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS view_post ON view (post, at);

             -- Requests refused as robots, counted per UTC day and per kind:
             -- `page`, `visit` or `view`. Nothing else about them is kept.
             CREATE TABLE IF NOT EXISTS robot (
                 day TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 dropped INTEGER NOT NULL,
                 PRIMARY KEY (day, kind)
             ) WITHOUT ROWID;",
        )
        .with_context(|| format!("Could not set up the schema in {}", settings.db.display()))?;

    let app = Arc::new(App {
        www: settings.www.clone(),
        posts: settings.posts.clone(),
        database: Mutex::new(connection),
    });

    let router = Router::new()
        .route("/", get(page))
        .route("/index.html", get(page))
        .route("/p/{post}", get(page))
        .route("/posts/{post}/{file}", get(media))
        .route("/api/visit", post(visit))
        .route("/api/posts", get(posts))
        .route("/api/views", post(views))
        .fallback_service(ServeDir::new(&settings.www))
        // Revalidate everything: ServeDir answers with Last-Modified, so an
        // unchanged file costs a 304, and a changed one shows up on the next
        // load without any cache-busting in the front end.
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(middleware::from_fn(access_log))
        .with_state(app);

    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), settings.port);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("Could not listen on {address}"))?;
    tracing::info!(
        msg = "Listening",
        address = %address,
        www = ?settings.www,
        posts = ?settings.posts,
        db = ?settings.db,
    );
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let mut terminate =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(terminate) => terminate,
                Err(error) => {
                    tracing::error!(msg = "Could not listen for SIGTERM", error = %error);
                    std::future::pending::<()>().await;
                    return;
                }
            };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
        tracing::info!(msg = "Shutting down");
    })
    .await?;
    Ok(())
}

/// `GET /`, `/index.html` and `/p/<n>`: `index.html` with the link-preview
/// tags filled in. The feed itself is drawn by the front end from
/// `/api/posts`; `{{post}}` tells it which post to open at.
async fn page(
    State(app): State<Arc<App>>,
    post: Option<Path<u32>>,
    headers: HeaderMap,
) -> Response {
    robot(&app, &headers, "page").await;

    let template = match tokio::fs::read_to_string(app.www.join("index.html")).await {
        Ok(template) => template,
        Err(error) => {
            tracing::error!(msg = "Could not read index.html", www = ?app.www, error = %error);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let mut status = StatusCode::OK;
    let mut title = CHANNEL_TITLE.to_string();
    let mut description = CHANNEL_DESCRIPTION.to_string();
    let mut image = CHANNEL_AVATAR.to_string();
    let mut focus = String::new();
    let mut path = "/".to_string();
    if let Some(Path(id)) = post {
        path = format!("/p/{id}");
        let directory = app.posts.clone();
        match tokio::task::spawn_blocking(move || read_post(&directory, id)).await {
            Ok(Ok(Some(found))) => {
                title = format!("{CHANNEL_TITLE} · {id}");
                description = found.excerpt;
                if let Some(found) = found.image {
                    image = found;
                }
                focus = id.to_string();
            }
            // The channel still opens, at its newest post: a dead link should
            // land somewhere readable.
            Ok(Ok(None)) => status = StatusCode::NOT_FOUND,
            Ok(Err(error)) => {
                tracing::error!(msg = "Could not read post", post = id, error = %format!("{error:#}"));
                status = StatusCode::INTERNAL_SERVER_ERROR;
            }
            Err(error) => {
                tracing::error!(msg = "Post reader panicked", post = id, error = %error);
                status = StatusCode::INTERNAL_SERVER_ERROR;
            }
        }
    }

    // Link previews need absolute URLs, and behind a proxy the scheme and host
    // the visitor used are only in the forwarding headers.
    let mut scheme = None;
    let mut host = None;
    if let Some(forwarded) = header_text(&headers, "forwarded")
        && let Some(first) = forwarded.split(',').next()
    {
        for pair in first.split(';') {
            if let Some((key, value)) = pair.trim().split_once('=') {
                let value = value.trim_matches('"').to_string();
                if key.eq_ignore_ascii_case("proto") {
                    scheme = Some(value);
                } else if key.eq_ignore_ascii_case("host") {
                    host = Some(value);
                }
            }
        }
    }
    if scheme.is_none()
        && let Some(value) = header_text(&headers, "x-forwarded-proto")
        && let Some(first) = value.split(',').next()
    {
        scheme = Some(first.trim().to_string());
    }
    if host.is_none()
        && let Some(value) = header_text(&headers, "x-forwarded-host")
        && let Some(first) = value.split(',').next()
    {
        host = Some(first.trim().to_string());
    }
    if host.is_none() {
        host = header_text(&headers, "host");
    }
    let origin = match (scheme, host) {
        (Some(scheme), Some(host)) => format!("{scheme}://{host}"),
        (None, Some(host)) => format!("http://{host}"),
        (_, None) => String::new(),
    };
    if image.starts_with('/') {
        image = format!("{origin}{image}");
    }
    let url = format!("{origin}{path}");

    let values = [
        ("channel_title", CHANNEL_TITLE),
        ("channel_description", CHANNEL_DESCRIPTION),
        ("channel_avatar", CHANNEL_AVATAR),
        ("title", title.as_str()),
        ("description", description.as_str()),
        ("image", image.as_str()),
        ("url", url.as_str()),
        ("post", focus.as_str()),
    ];
    // One pass over the template, so a `{{…}}` inside a post excerpt is text,
    // never a second substitution. Unknown names are left as written.
    let mut html = String::with_capacity(template.len() + 1024);
    let mut rest = template.as_str();
    while let Some(start) = rest.find("{{") {
        html.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            html.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let name = &after[..end];
        let mut value = None;
        for (key, found) in values {
            if key == name {
                value = Some(found);
            }
        }
        match value {
            Some(value) => {
                for character in value.chars() {
                    match character {
                        '&' => html.push_str("&amp;"),
                        '<' => html.push_str("&lt;"),
                        '>' => html.push_str("&gt;"),
                        '"' => html.push_str("&quot;"),
                        '\'' => html.push_str("&#39;"),
                        other => html.push(other),
                    }
                }
            }
            None => html.push_str(&rest[start..start + 2 + end + 2]),
        }
        rest = &after[end + 2..];
    }
    html.push_str(rest);
    (status, Html(html)).into_response()
}

/// `GET /posts/<n>/<file>`: a file from a post's folder. `post.md` itself and
/// dotfiles are not served, and `<n>` must be a number, so nothing else under
/// `--posts` — a `.git`, a README — is reachable.
async fn media(
    State(app): State<Arc<App>>,
    Path((post, file)): Path<(String, String)>,
    request: Request,
) -> Response {
    if post.is_empty()
        || !post.bytes().all(|byte| byte.is_ascii_digit())
        || file.is_empty()
        || file.starts_with('.')
        || file.contains(['/', '\\'])
        || file == "post.md"
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    match ServeFile::new(app.posts.join(&post).join(&file))
        .try_call(request)
        .await
    {
        Ok(response) => response.map(Body::new),
        Err(error) => {
            tracing::error!(msg = "Could not serve post file", post = %post, file = %file, error = %error);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[derive(Deserialize)]
struct VisitRequest {
    /// The ULID this browser was given before, if it kept one.
    device: Option<String>,
    /// `location.pathname` of the page being opened.
    path: String,
    /// `document.referrer`: where the visitor came from. The Referer header of
    /// this request would only name our own page.
    referrer: String,
}

#[derive(Serialize)]
struct VisitResponse {
    device: String,
    /// Every device ever issued, shown as the channel's subscriber count.
    subscribers: u64,
}

/// `POST /api/visit`, once per page open: the handshake. An unknown or missing
/// device gets a new ULID; either way one `visit` row is written. Robots get a
/// `204` and no device, so the front end never sends views for them.
async fn visit(
    State(app): State<Arc<App>>,
    ConnectInfo(socket): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<VisitRequest>,
) -> Response {
    if robot(&app, &headers, "visit").await {
        return StatusCode::NO_CONTENT.into_response();
    }

    // The right-most entry of each header is the one our own proxy appended,
    // which the client cannot forge; anything to its left can be.
    let mut ip = None;
    if let Some(forwarded) = header_text(&headers, "forwarded")
        && let Some(last) = forwarded.rsplit(',').next()
    {
        for pair in last.split(';') {
            if let Some((key, value)) = pair.trim().split_once('=')
                && key.eq_ignore_ascii_case("for")
            {
                // `for="[2001:db8::1]:4711"`, `for=192.0.2.1:80` or bare.
                let value = value.trim_matches('"');
                let value = match value.strip_prefix('[') {
                    Some(bracketed) => match bracketed.split_once(']') {
                        Some((inside, _)) => inside,
                        None => bracketed,
                    },
                    None => match value.split_once(':') {
                        Some((address, _)) if !value[address.len() + 1..].contains(':') => address,
                        _ => value,
                    },
                };
                if let Ok(parsed) = value.parse::<IpAddr>() {
                    ip = Some(parsed);
                }
            }
        }
    }
    if ip.is_none()
        && let Some(value) = header_text(&headers, "x-forwarded-for")
        && let Some(last) = value.rsplit(',').next()
        && let Ok(parsed) = last.trim().parse::<IpAddr>()
    {
        ip = Some(parsed);
    }
    if ip.is_none()
        && let Some(value) = header_text(&headers, "x-real-ip")
        && let Ok(parsed) = value.trim().parse::<IpAddr>()
    {
        ip = Some(parsed);
    }
    let ip = match ip {
        Some(ip) => ip,
        None => socket.ip(),
    };

    let user_agent = match header_text(&headers, "user-agent") {
        Some(agent) => clip(&agent),
        None => String::new(),
    };
    let language = match header_text(&headers, "accept-language") {
        Some(language) => clip(&language),
        None => String::new(),
    };
    let referrer = clip(&body.referrer);
    let path = clip(&body.path);
    let claimed = match &body.device {
        Some(text) => Ulid::parse(text),
        None => None,
    };
    let fresh = match Ulid::new() {
        Ok(fresh) => fresh,
        Err(error) => {
            tracing::error!(msg = "Could not make a device id", error = %error);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let result = database(&app, move |connection| {
        let transaction = connection.unchecked_transaction()?;
        let mut device = None;
        if let Some(claimed) = claimed {
            let known: Option<i64> = transaction
                .query_row(
                    "SELECT 1 FROM device WHERE id = ?1",
                    [&claimed.0[..]],
                    |row| row.get(0),
                )
                .optional()?;
            if known.is_some() {
                device = Some(claimed);
            }
        }
        let device = match device {
            Some(device) => device,
            None => {
                transaction.execute(
                    "INSERT INTO device (id, created_at) VALUES (?1, ?2)",
                    params![&fresh.0[..], now()],
                )?;
                fresh
            }
        };
        transaction.execute(
            "INSERT INTO visit (device, at, ip, user_agent, referrer, language, path)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                &device.0[..],
                now(),
                ip.to_string(),
                user_agent,
                referrer,
                language,
                path
            ],
        )?;
        let subscribers: i64 =
            transaction.query_row("SELECT count(*) FROM device", [], |row| row.get(0))?;
        transaction.commit()?;
        Ok((device, subscribers))
    })
    .await;

    match result {
        Ok((device, subscribers)) => Json(VisitResponse {
            device: device.to_string(),
            subscribers: subscribers as u64,
        })
        .into_response(),
        Err(error) => {
            tracing::error!(msg = "Could not record visit", error = %format!("{error:#}"));
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[derive(Deserialize)]
struct PostsQuery {
    /// The page of posts just older than this one.
    before: Option<u32>,
    /// The page of posts just newer than this one.
    after: Option<u32>,
    /// A page with this post in the middle, for `/p/<n>`.
    around: Option<u32>,
}

#[derive(Serialize)]
struct PostsResponse {
    /// Oldest first.
    posts: Vec<Post>,
    /// Whether there are posts before the first one here.
    older: bool,
    /// Whether there are posts after the last one here.
    newer: bool,
}

/// `GET /api/posts`: one page of posts, newest page when no cursor is given.
/// The directory is listed and the posts read on every call.
async fn posts(State(app): State<Arc<App>>, Query(query): Query<PostsQuery>) -> Response {
    let directory = app.posts.clone();
    let read = tokio::task::spawn_blocking(move || -> anyhow::Result<PostsResponse> {
        let mut ids = Vec::new();
        let entries = std::fs::read_dir(&directory)
            .with_context(|| format!("Could not list {}", directory.display()))?;
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if let Ok(id) = name.parse::<u32>()
                && entry.path().join("post.md").is_file()
            {
                ids.push(id);
            }
        }
        ids.sort_unstable();

        let (start, end) = if let Some(before) = query.before {
            let end = ids.partition_point(|id| *id < before);
            (end.saturating_sub(PAGE_SIZE), end)
        } else if let Some(after) = query.after {
            let start = ids.partition_point(|id| *id <= after);
            (start, (start + PAGE_SIZE).min(ids.len()))
        } else if let Some(around) = query.around {
            let middle = ids.partition_point(|id| *id < around);
            let start = middle.saturating_sub(PAGE_SIZE / 2);
            (start, (start + PAGE_SIZE).min(ids.len()))
        } else {
            (ids.len().saturating_sub(PAGE_SIZE), ids.len())
        };

        let mut posts = Vec::new();
        for id in &ids[start..end] {
            // One broken post must not take the channel down with it.
            match read_post(&directory, *id) {
                Ok(Some(post)) => posts.push(post),
                Ok(None) => {}
                Err(error) => {
                    tracing::error!(msg = "Skipped unreadable post", post = id, error = %format!("{error:#}"));
                }
            }
        }
        Ok(PostsResponse {
            posts,
            older: start > 0,
            newer: end < ids.len(),
        })
    })
    .await;

    let mut response = match read {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            tracing::error!(msg = "Could not read posts", error = %format!("{error:#}"));
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        Err(error) => {
            tracing::error!(msg = "Posts reader panicked", error = %error);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let mut ids = Vec::new();
    for post in &response.posts {
        ids.push(post.id);
    }
    match database(&app, move |connection| view_counts(connection, &ids)).await {
        Ok(counts) => {
            for post in &mut response.posts {
                if let Some(count) = counts.get(&post.id) {
                    post.views = *count;
                }
            }
        }
        Err(error) => {
            // The posts are still worth showing without their numbers.
            tracing::error!(msg = "Could not count views", error = %format!("{error:#}"));
        }
    }
    Json(response).into_response()
}

#[derive(Deserialize)]
struct ViewsRequest {
    device: String,
    posts: Vec<u32>,
}

#[derive(Serialize)]
struct ViewsResponse {
    /// The new total for every post that was counted.
    views: HashMap<u32, u64>,
}

/// `POST /api/views`: posts this device has had on screen for two seconds.
/// The front end sends each post at most once per page load; every call here
/// is a new row per post, so coming back later counts again.
async fn views(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(body): Json<ViewsRequest>,
) -> Response {
    if robot(&app, &headers, "view").await {
        return StatusCode::NO_CONTENT.into_response();
    }
    let Some(device) = Ulid::parse(&body.device) else {
        return (StatusCode::BAD_REQUEST, "device is not a ULID").into_response();
    };
    // A page holds a few pages of posts; more than this is not a browser.
    if body.posts.len() > PAGE_SIZE * 5 {
        return (StatusCode::BAD_REQUEST, "too many posts").into_response();
    }
    let mut ids = Vec::new();
    for id in body.posts {
        if !ids.contains(&id) && app.posts.join(id.to_string()).join("post.md").is_file() {
            ids.push(id);
        }
    }

    let result = database(&app, move |connection| {
        let known: Option<i64> = connection
            .query_row(
                "SELECT 1 FROM device WHERE id = ?1",
                [&device.0[..]],
                |row| row.get(0),
            )
            .optional()?;
        if known.is_none() {
            return Ok(None);
        }
        let transaction = connection.unchecked_transaction()?;
        let at = now();
        for id in &ids {
            transaction.execute(
                "INSERT INTO view (post, device, at) VALUES (?1, ?2, ?3)",
                params![id, &device.0[..], at],
            )?;
        }
        transaction.commit()?;
        Ok(Some(view_counts(connection, &ids)?))
    })
    .await;

    match result {
        Ok(Some(views)) => Json(ViewsResponse { views }).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "unknown device").into_response(),
        Err(error) => {
            tracing::error!(msg = "Could not record views", error = %format!("{error:#}"));
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// One line per request, at a level that follows the status: 2xx and 3xx
/// `info`, 4xx `warn`, 5xx `error`.
async fn access_log(request: Request, next: Next) -> Response {
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    let query = match request.uri().query() {
        Some(query) => query.to_string(),
        None => String::new(),
    };
    let route = match request.extensions().get::<MatchedPath>() {
        Some(matched) => matched.as_str().to_string(),
        None => path.clone(),
    };
    let started = Instant::now();
    let response = next.run(request).await;
    let status = response.status().as_u16();
    let total_ms = started.elapsed().as_secs_f64() * 1000.0;
    macro_rules! report {
        ($level:ident, $said:literal) => {
            tracing::$level!(
                msg = $said,
                http.request.method = method.as_str(),
                http.route = route.as_str(),
                url.path = path.as_str(),
                url.query = query.as_str(),
                http.response.status_code = status,
                total_ms = total_ms,
            )
        };
    }
    match status {
        500.. => report!(error, "Failed request"),
        400..500 => report!(warn, "Rejected request"),
        _ => report!(info, "Handled request"),
    }
    response
}

/// Read and render `<posts>/<id>/post.md`. `None` when the folder or the file
/// is not there.
///
/// The file opens with a header and then Markdown:
///
/// ```text
/// ---
/// date: 2026-10-19 14:30
/// ---
/// Hello, this is the post.
/// ![](cat.jpg)
/// ```
fn read_post(directory: &FilePath, id: u32) -> anyhow::Result<Option<Post>> {
    let path = directory.join(id.to_string()).join("post.md");
    let source = match std::fs::read_to_string(&path) {
        Ok(source) => source.replace("\r\n", "\n"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("Could not read {}", path.display()));
        }
    };
    let Some(rest) = source.strip_prefix("---\n") else {
        bail!(
            "{} must open with a `---` line, then `date: YYYY-MM-DD HH:MM`, then `---`",
            path.display()
        );
    };
    let Some((header, body)) = rest.split_once("\n---\n") else {
        bail!("{} has no `---` line closing its header", path.display());
    };
    let mut date = None;
    for line in header.lines() {
        if let Some((key, value)) = line.split_once(':')
            && key.trim() == "date"
        {
            date = Some(value.trim());
        }
    }
    let Some(date) = date else {
        bail!("{} has no `date:` in its header", path.display());
    };
    // `YYYY-MM-DD HH:MM`, checked by position rather than parsed into a type:
    // the front end only ever reads it back as text.
    let bytes = date.as_bytes();
    let mut valid = bytes.len() == 16
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && (bytes[10] == b' ' || bytes[10] == b'T')
        && bytes[13] == b':';
    for position in [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15] {
        if valid && !bytes[position].is_ascii_digit() {
            valid = false;
        }
    }
    if valid {
        let month: u32 = date[5..7].parse()?;
        let day: u32 = date[8..10].parse()?;
        let hour: u32 = date[11..13].parse()?;
        let minute: u32 = date[14..16].parse()?;
        valid = (1..=12).contains(&month) && (1..=31).contains(&day) && hour < 24 && minute < 60;
    }
    if !valid {
        bail!(
            "{}: date `{date}` is not `YYYY-MM-DD HH:MM`",
            path.display()
        );
    }
    let date = format!("{}T{}", &date[..10], &date[11..]);

    // A relative link or image points into the post's own folder, which is
    // served at /posts/<id>/. Anything with a scheme, or starting with `/`,
    // `#` or `?`, is left alone.
    let local = |url: CowStr<'static>| -> CowStr<'static> {
        let scheme = match (url.find(':'), url.find('/')) {
            (Some(colon), Some(slash)) => colon < slash,
            (Some(_), None) => true,
            (None, _) => false,
        };
        if scheme || url.is_empty() || url.starts_with(['/', '#', '?']) {
            return url;
        }
        let url = match url.strip_prefix("./") {
            Some(stripped) => stripped.to_string(),
            None => url.to_string(),
        };
        CowStr::from(format!("/posts/{id}/{url}"))
    };

    let mut excerpt = String::new();
    let mut image = None;
    let mut events = Vec::new();
    let parser = pulldown_cmark::Parser::new_ext(
        body,
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS,
    );
    for event in parser {
        let event = match event {
            // Written like a Telegram message: Enter is a new line.
            Event::SoftBreak => {
                excerpt.push(' ');
                Event::HardBreak
            }
            // A block's end is a word boundary in the excerpt too, or two
            // paragraphs read as one run-on word in a link preview.
            Event::HardBreak
            | Event::End(
                TagEnd::Paragraph
                | TagEnd::Heading(_)
                | TagEnd::BlockQuote(_)
                | TagEnd::CodeBlock
                | TagEnd::Item
                | TagEnd::TableCell,
            ) => {
                excerpt.push(' ');
                event
            }
            Event::Text(text) => {
                excerpt.push_str(&text);
                Event::Text(text)
            }
            Event::Code(text) => {
                excerpt.push_str(&text);
                Event::Code(text)
            }
            Event::Start(Tag::Image {
                link_type,
                dest_url,
                title,
                id: reference,
            }) => {
                let dest_url = local(dest_url.into_static());
                if image.is_none() {
                    image = Some(dest_url.to_string());
                }
                Event::Start(Tag::Image {
                    link_type,
                    dest_url,
                    title,
                    id: reference,
                })
            }
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id: reference,
            }) => Event::Start(Tag::Link {
                link_type,
                dest_url: local(dest_url.into_static()),
                title,
                id: reference,
            }),
            other => other,
        };
        events.push(event);
    }
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, events.into_iter());

    let mut words = String::new();
    for word in excerpt.split_whitespace() {
        if !words.is_empty() {
            words.push(' ');
        }
        words.push_str(word);
    }
    let excerpt = match words.char_indices().nth(200) {
        Some((cut, _)) => format!("{}…", &words[..cut]),
        None => words,
    };

    Ok(Some(Post {
        id,
        date,
        html,
        views: 0,
        excerpt,
        image,
    }))
}

/// Total views of each post in `ids` that has any.
fn view_counts(connection: &Connection, ids: &[u32]) -> rusqlite::Result<HashMap<u32, u64>> {
    let mut counts = HashMap::new();
    if ids.is_empty() {
        return Ok(counts);
    }
    let mut placeholders = String::new();
    for index in 0..ids.len() {
        if index > 0 {
            placeholders.push(',');
        }
        placeholders.push('?');
    }
    let mut statement = connection.prepare(&format!(
        "SELECT post, count(*) FROM view WHERE post IN ({placeholders}) GROUP BY post"
    ))?;
    let mut rows = statement.query(params_from_iter(ids))?;
    while let Some(row) = rows.next()? {
        let post: u32 = row.get(0)?;
        let count: i64 = row.get(1)?;
        counts.insert(post, count as u64);
    }
    Ok(counts)
}

/// Whether this request looks like a robot, and if so, count it under `kind`
/// for today. Browsers all send a User-Agent starting `Mozilla/`; robots
/// mostly send none, their own name, or a library's.
async fn robot(app: &Arc<App>, headers: &HeaderMap, kind: &'static str) -> bool {
    let agent = match header_text(headers, "user-agent") {
        Some(agent) => agent.to_ascii_lowercase(),
        None => String::new(),
    };
    let mut robot = !agent.starts_with("mozilla/");
    for marker in ROBOT_MARKERS {
        if agent.contains(marker) {
            robot = true;
        }
    }
    if !robot {
        return false;
    }
    tracing::debug!(msg = "Dropped robot", kind = kind, user_agent = %agent);
    let counted = database(app, move |connection| {
        connection.execute(
            "INSERT INTO robot (day, kind, dropped) VALUES (date('now'), ?1, 1)
             ON CONFLICT (day, kind) DO UPDATE SET dropped = dropped + 1",
            [kind],
        )
    })
    .await;
    if let Err(error) = counted {
        tracing::error!(msg = "Could not count robot", kind = kind, error = %format!("{error:#}"));
    }
    true
}

/// Run `work` against the database on a blocking thread.
async fn database<T, F>(app: &Arc<App>, work: F) -> anyhow::Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
{
    let app = app.clone();
    let joined = tokio::task::spawn_blocking(move || {
        // A panic while holding the lock leaves SQLite itself consistent: any
        // open transaction is rolled back when it is dropped.
        let connection = match app.database.lock() {
            Ok(connection) => connection,
            Err(poisoned) => poisoned.into_inner(),
        };
        work(&connection)
    })
    .await;
    match joined {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(error.into()),
        Err(error) => Err(anyhow::anyhow!("Database task panicked: {error}")),
    }
}

/// A header's value as text, or `None` when absent or not visible ASCII.
fn header_text(headers: &HeaderMap, name: &str) -> Option<String> {
    match headers.get(name) {
        Some(value) => match value.to_str() {
            Ok(text) => Some(text.to_string()),
            Err(_) => None,
        },
        None => None,
    }
}

/// At most [`TEXT_LIMIT`] characters of `text`.
fn clip(text: &str) -> String {
    match text.char_indices().nth(TEXT_LIMIT) {
        Some((cut, _)) => text[..cut].to_string(),
        None => text.to_string(),
    }
}

/// Unix milliseconds. Saturates to 0 on a clock before 1970: a timestamp is
/// not worth refusing a write over.
fn now() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => elapsed.as_millis() as i64,
        Err(_) => 0,
    }
}
