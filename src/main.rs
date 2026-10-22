//! plogia: a blog that looks and reads like a Telegram channel.
//!
//! Every request reads what it needs from disk — `--www` for the front end,
//! `--posts` for the posts — and nothing about either is kept in memory, so
//! publishing is writing a file. SQLite (`--db`) holds who came and what they
//! read: one row per device, per visit and per view, and a per-day counter of
//! robots turned away. It also keeps the email subscribers, what post went to
//! whom, and the DKIM key the emails are signed with: each post is sent
//! straight to every subscriber's mail server, no relay in between.

use std::{
    collections::{HashMap, VecDeque},
    fmt,
    fmt::Write,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path as FilePath, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
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
use hickory_resolver::{TokioResolver, proto::rr::RData};
use lettre::{
    Address, AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
    message::{
        Mailbox, MultiPart,
        dkim::{DkimConfig, DkimSigningAlgorithm, DkimSigningKey},
    },
    transport::smtp::{
        client::{Tls, TlsParameters},
        extension::ClientId,
    },
};
use pulldown_cmark::{CowStr, Event, Options, Tag, TagEnd};
use rsa::{
    RsaPrivateKey, RsaPublicKey,
    pkcs1::{DecodeRsaPrivateKey, EncodeRsaPrivateKey},
    pkcs8::{EncodePublicKey, LineEnding},
};
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

/// In the channel info, the description of a link to the channel itself and
/// the RSS feed's, in Persian and in English.
const CHANNEL_DESCRIPTION_FA: &str = "گفته‌های پوریا";
const CHANNEL_DESCRIPTION_EN: &str = "Pouriya's Thoughts";

/// How to reach the author, under the description in the channel info.
const CHANNEL_CONTACT: &str = "https://pouriya.net/contact";

/// A path under `--www`: the avatar in the header, beside every post and in
/// the channel info. Link previews carry no image at all.
const CHANNEL_AVATAR: &str = "/plogia.jpg";

/// The same channel on Telegram, for readers who would rather follow it there:
/// on the Join panel and in the channel info.
const CHANNEL_TELEGRAM: &str = "https://t.me/plogia";

/// This code, in the channel info.
const CHANNEL_GITHUB: &str = "https://github.com/pouriya/plogia";

/// Every post email's subject opens with this, then ` · ` and the post's first
/// sentence, so a subscriber finds them all with one search. Both
/// descriptions, as every email carries both languages.
const EMAIL_TITLE: &str = "گفته‌های پوریا / Pouriya's Thoughts";

/// Post emails come from `CHANNEL_TITLE <EMAIL_USER@host of --url>`.
const EMAIL_USER: &str = "noreply";

/// The DKIM key is published at `<DKIM_SELECTOR>._domainkey.<host of --url>`.
const DKIM_SELECTOR: &str = "plogia";

/// The zone of every post's `date`, which is written without one: the
/// author's wall clock. Only used for `article:published_time`.
const TIMEZONE: &str = "+03:30";

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

    /// One folder per post, named by its number: `<n>/fa.md`, `<n>/en.md` or
    /// both, and the files they link to, which are served under `/posts/<n>/`.
    #[arg(long, value_name = "DIRECTORY")]
    posts: PathBuf,

    /// SQLite database for devices, visits, views, robot counters,
    /// subscribers, deliveries and the DKIM key. Created if missing.
    #[arg(long, value_name = "FILE")]
    db: PathBuf,

    /// Where readers reach the channel, like `https://plogia.ir`: link
    /// previews, the RSS feed and post emails link there. Its host is also the
    /// domain emails are sent from and signed for.
    #[arg(long, value_name = "URL")]
    url: String,

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
    /// `--url` without a trailing `/`.
    url: String,
    /// The host of `--url`, without a port.
    host: String,
    database: Mutex<Connection>,
    /// Captchas shown and not yet answered: id, answer, when made. At most
    /// 1000, oldest first; each is good for 10 minutes and one try.
    captchas: Mutex<VecDeque<(String, String, Instant)>>,
    /// Who was given a captcha in the last second, at most 100 of them.
    recent: Mutex<Vec<(IpAddr, Instant)>>,
    /// Wakes the email sender when a new subscriber is owed a welcome.
    wake: tokio::sync::Notify,
}

/// A post as `GET /api/posts` returns it: `fa`, `en` or both, never neither.
#[derive(Serialize)]
struct Post {
    id: u32,
    /// `YYYY-MM-DDTHH:MM`, the author's wall clock, no zone.
    date: String,
    /// `fa.md`, when the post has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    fa: Option<Version>,
    /// `en.md`, when the post has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    en: Option<Version>,
    views: u64,
}

/// One language of a post. Sent to the front end as its HTML alone.
#[derive(Serialize)]
#[serde(transparent)]
struct Version {
    html: String,
    /// First sentence, the title of the link preview of `/p/<n>`.
    #[serde(skip)]
    sentence: String,
    /// Plain text after the first sentence, its description.
    #[serde(skip)]
    excerpt: String,
    /// The whole post as plain text, the text part of its email.
    #[serde(skip)]
    text: String,
}

impl Post {
    /// The version in `language`, or the other one when the post lacks it,
    /// with the code of the one returned.
    fn pick(&self, language: &str) -> Option<(&'static str, &Version)> {
        match (&self.fa, &self.en) {
            (Some(_), Some(en)) if language == "en" => Some(("en", en)),
            (Some(fa), _) => Some(("fa", fa)),
            (None, Some(en)) => Some(("en", en)),
            (None, None) => None,
        }
    }
}

/// `?l=fa` or `?l=en` on `/`, `/p/<n>` and `/rss.xml`: the language of the
/// link preview or the feed. Missing or anything else is Persian.
#[derive(Deserialize)]
struct LanguageQuery {
    l: Option<String>,
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
    let url = settings.url.trim_end_matches('/').to_string();
    let host = match url.split_once("://") {
        Some(("http" | "https", rest)) => {
            let authority = match rest.split_once('/') {
                Some((authority, _)) => authority,
                None => rest,
            };
            match authority.rsplit_once(':') {
                Some((name, port)) if port.parse::<u16>().is_ok() => name.to_string(),
                _ => authority.to_string(),
            }
        }
        _ => bail!("--url {} must start with http:// or https://", settings.url),
    };
    if host.is_empty() {
        bail!("--url {} has no host", settings.url);
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
             -- `page`, `visit`, `view`, `captcha` or `subscribe`. Nothing else
             -- about them is kept.
             CREATE TABLE IF NOT EXISTS robot (
                 day TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 dropped INTEGER NOT NULL,
                 PRIMARY KEY (day, kind)
             ) WITHOUT ROWID;

             -- One row per email given on the Join panel, kept as first typed.
             -- No confirmation and no removal yet.
             CREATE TABLE IF NOT EXISTS subscriber (
                 id INTEGER PRIMARY KEY,
                 email TEXT NOT NULL UNIQUE COLLATE NOCASE,
                 ip TEXT NOT NULL,
                 at INTEGER NOT NULL
             );

             -- What post went to whom. `skipped`: the post was already out
             -- when they subscribed. `pending`: owed, due at `next_at`.
             -- `sent`, or `failed` with its `error`: done, never tried again.
             -- `at` is the last change.
             CREATE TABLE IF NOT EXISTS delivery (
                 post INTEGER NOT NULL,
                 subscriber INTEGER NOT NULL REFERENCES subscriber (id),
                 status TEXT NOT NULL,
                 attempts INTEGER NOT NULL,
                 next_at INTEGER,
                 error TEXT,
                 at INTEGER NOT NULL,
                 PRIMARY KEY (post, subscriber)
             ) WITHOUT ROWID;
             CREATE INDEX IF NOT EXISTS delivery_due ON delivery (status, next_at);

             -- Secrets made on first start: `dkim`, the PEM of the key every
             -- email is signed with.
             CREATE TABLE IF NOT EXISTS key (
                 name TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             ) WITHOUT ROWID;",
        )
        .with_context(|| format!("Could not set up the schema in {}", settings.db.display()))?;

    // The DKIM key is made once and kept, since its public half is published
    // in DNS; that record is logged at every start, for whoever sets DNS up.
    let stored: Option<String> = connection
        .query_row("SELECT value FROM key WHERE name = 'dkim'", [], |row| {
            row.get(0)
        })
        .optional()
        .context("Could not read the DKIM key")?;
    let (private, pem) = match stored {
        Some(pem) => {
            let private = RsaPrivateKey::from_pkcs1_pem(&pem)
                .context("The stored DKIM key is not an RSA key")?;
            (private, pem)
        }
        None => {
            tracing::info!(msg = "Generating the DKIM key", bits = 2048);
            let private = RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048)
                .context("Could not generate the DKIM key")?;
            let pem = private
                .to_pkcs1_pem(LineEnding::LF)
                .context("Could not write the DKIM key")?
                .to_string();
            connection
                .execute("INSERT INTO key (name, value) VALUES ('dkim', ?1)", [&pem])
                .context("Could not store the DKIM key")?;
            (private, pem)
        }
    };
    let public = RsaPublicKey::from(&private)
        .to_public_key_pem(LineEnding::LF)
        .context("Could not write the DKIM public key")?;
    let mut record = String::new();
    for line in public.lines() {
        if !line.starts_with("-----") {
            record.push_str(line);
        }
    }
    tracing::info!(
        msg = "Publish this DKIM record",
        name = %format!("{DKIM_SELECTOR}._domainkey.{host}"),
        r#type = "TXT",
        value = %format!("v=DKIM1; k=rsa; p={record}"),
    );
    let signing = match DkimSigningKey::new(&pem, DkimSigningAlgorithm::Rsa) {
        Ok(signing) => signing,
        Err(error) => bail!("Could not load the DKIM key: {error:?}"),
    };
    let dkim = DkimConfig::default_config(DKIM_SELECTOR.to_string(), host.clone(), signing);

    let app = Arc::new(App {
        www: settings.www.clone(),
        posts: settings.posts.clone(),
        url,
        host,
        database: Mutex::new(connection),
        captchas: Mutex::new(VecDeque::new()),
        recent: Mutex::new(Vec::new()),
        wake: tokio::sync::Notify::new(),
    });

    // Emails. At start, every subscriber is owed each post that has no
    // delivery row for them yet — the posts already out when they subscribed
    // were marked `skipped` then — and those rows become `pending`; a new
    // subscriber's welcome is a `pending` row for post -1, made when they
    // subscribe. The due ones go out one at a time, oldest first; a 4xx answer
    // is tried again after 5 minutes, 30 minutes and 2 hours, anything else is
    // final. With nothing due the task waits for a new subscriber or the next
    // retry: new posts come with a restart.
    let sender = app.clone();
    tokio::spawn(async move {
        let app = sender;
        let directory = app.posts.clone();
        let listed = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u32>> {
            let mut ready = Vec::new();
            for id in post_ids(&directory)? {
                // Not sent while it does not parse; fixed, it goes out on the
                // next start.
                match read_post(&directory, id) {
                    Ok(Some(_)) => ready.push(id),
                    Ok(None) => {}
                    Err(error) => {
                        tracing::error!(msg = "Skipped unreadable post", post = id, error = %format!("{error:#}"));
                    }
                }
            }
            Ok(ready)
        })
        .await;
        let ids = match listed {
            Ok(Ok(ids)) => ids,
            Ok(Err(error)) => {
                tracing::error!(msg = "Could not list posts to email", error = %format!("{error:#}"));
                return;
            }
            Err(error) => {
                tracing::error!(msg = "Posts reader panicked", error = %error);
                return;
            }
        };
        let queued = database(&app, move |connection| {
            let transaction = connection.unchecked_transaction()?;
            let at = now();
            let mut subscribers = Vec::new();
            {
                let mut statement = transaction.prepare("SELECT id, email FROM subscriber")?;
                let mut rows = statement.query([])?;
                while let Some(row) = rows.next()? {
                    subscribers.push((row.get::<_, i64>(0)?, row.get::<_, String>(1)?));
                }
            }
            let mut queued = 0;
            let mut skipped = 0;
            for id in &ids {
                for (subscriber, email) in &subscribers {
                    if noreply(email) {
                        let added = transaction.execute(
                            "INSERT OR IGNORE INTO delivery (post, subscriber, status, attempts, next_at, error, at)
                             VALUES (?1, ?2, 'skipped', 0, NULL, 'noreply address', ?3)",
                            params![id, subscriber, at],
                        )?;
                        if added == 1 {
                            tracing::warn!(msg = "Skipped post email to a noreply address", post = id, email = %email);
                        }
                        skipped += added;
                    } else {
                        queued += transaction.execute(
                            "INSERT OR IGNORE INTO delivery (post, subscriber, status, attempts, next_at, error, at)
                             VALUES (?1, ?2, 'pending', 0, ?3, NULL, ?3)",
                            params![id, subscriber, at],
                        )?;
                    }
                }
            }
            transaction.commit()?;
            Ok((queued, skipped))
        })
        .await;
        match queued {
            Ok((queued, skipped)) => {
                tracing::info!(
                    msg = "Queued post emails",
                    emails = queued,
                    skipped = skipped
                );
            }
            Err(error) => {
                tracing::error!(msg = "Could not queue post emails", error = %format!("{error:#}"));
                return;
            }
        }

        let resolver = match TokioResolver::builder_tokio() {
            Ok(builder) => match builder.build() {
                Ok(resolver) => resolver,
                Err(error) => {
                    tracing::error!(msg = "Could not set up DNS", error = %error);
                    return;
                }
            },
            Err(error) => {
                tracing::error!(msg = "Could not read the DNS configuration", error = %error);
                return;
            }
        };

        loop {
            let due = database(&app, |connection| {
                connection
                    .query_row(
                        "SELECT delivery.post, delivery.subscriber, delivery.attempts,
                                delivery.next_at, subscriber.email
                         FROM delivery JOIN subscriber ON subscriber.id = delivery.subscriber
                         WHERE delivery.status = 'pending'
                         ORDER BY delivery.next_at, delivery.post
                         LIMIT 1",
                        [],
                        |row| {
                            Ok((
                                row.get::<_, i64>(0)?,
                                row.get::<_, i64>(1)?,
                                row.get::<_, i64>(2)?,
                                row.get::<_, i64>(3)?,
                                row.get::<_, String>(4)?,
                            ))
                        },
                    )
                    .optional()
            })
            .await;
            let (post, subscriber, attempts, next_at, email) = match due {
                Ok(Some(due)) => due,
                Ok(None) => {
                    tracing::debug!(msg = "No emails due, waiting for a subscriber");
                    app.wake.notified().await;
                    continue;
                }
                Err(error) => {
                    tracing::error!(msg = "Could not read pending emails", error = %format!("{error:#}"));
                    return;
                }
            };
            // A welcome must not wait behind a retry hours away.
            let wait = next_at - now();
            if wait > 0 {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(wait as u64)) => {}
                    _ = app.wake.notified() => {}
                }
                continue;
            }

            let kind = if post < 0 { "welcome" } else { "post" };
            // Rows owed from before the rule existed.
            if noreply(&email) {
                tracing::warn!(msg = "Skipped email to a noreply address", kind = kind, post = post, email = %email);
                let at = now();
                let recorded = database(&app, move |connection| {
                    connection.execute(
                        "UPDATE delivery
                         SET status = 'skipped', next_at = NULL, error = 'noreply address', at = ?1
                         WHERE post = ?2 AND subscriber = ?3",
                        params![at, post, subscriber],
                    )
                })
                .await;
                if let Err(error) = recorded {
                    tracing::error!(msg = "Could not record email, stopped sending", kind = kind, post = post, email = %email, error = %format!("{error:#}"));
                    return;
                }
                continue;
            }
            tracing::debug!(msg = "Sending email", kind = kind, post = post, email = %email, attempt = attempts + 1);
            // The error and whether it is worth another try (a 4xx answer).
            let outcome: Result<String, (String, bool)> = 'send: {
                let (subject, html, text) = if post < 0 {
                    // Each half links to the channel and the feed in its own
                    // language.
                    let channel_fa = format!("{}/?l=fa", app.url);
                    let channel_en = format!("{}/?l=en", app.url);
                    let rss_fa = format!("{}/rss.xml?l=fa", app.url);
                    let rss_en = format!("{}/rss.xml?l=en", app.url);
                    let mut title = String::new();
                    escape(CHANNEL_TITLE, &mut title);
                    (
                        format!("{EMAIL_TITLE} · عضو شدید / You're subscribed"),
                        format!(
                            "<!doctype html>\n<html><body>\n\
                             <div dir=\"rtl\" lang=\"fa\">\n\
                             <p>سلام،<br>\nبه {title} پیوستید. هر پستی که از این پس منتشر شود، به همین نشانی برایتان فرستاده می‌شود.</p>\n\
                             <p><a href=\"{channel_fa}\">کانال</a> · <a href=\"{CHANNEL_TELEGRAM}\">تلگرام</a> · <a href=\"{rss_fa}\">خوراک RSS</a></p>\n\
                             </div>\n<hr>\n\
                             <div dir=\"ltr\" lang=\"en\">\n\
                             <p>Hi,<br>\nYou're subscribed to {title}. Every new post will come to this address.</p>\n\
                             <p><a href=\"{channel_en}\">Channel</a> · <a href=\"{CHANNEL_TELEGRAM}\">Telegram</a> · <a href=\"{rss_en}\">RSS</a></p>\n\
                             </div>\n</body></html>\n"
                        ),
                        format!(
                            "سلام،\nبه {CHANNEL_TITLE} پیوستید. هر پستی که از این پس منتشر شود، به همین نشانی برایتان فرستاده می‌شود.\n\n\
                             کانال: {channel_fa}\nتلگرام: {CHANNEL_TELEGRAM}\nخوراک RSS: {rss_fa}\n\n\
                             ————————\n\n\
                             Hi,\nYou're subscribed to {CHANNEL_TITLE}. Every new post will come to this address.\n\n\
                             Channel: {channel_en}\nTelegram: {CHANNEL_TELEGRAM}\nRSS: {rss_en}\n"
                        ),
                    )
                } else {
                    let directory = app.posts.clone();
                    let id = post as u32;
                    let found = match tokio::task::spawn_blocking(move || read_post(&directory, id))
                        .await
                    {
                        Ok(Ok(Some(found))) => found,
                        Ok(Ok(None)) => {
                            break 'send Err(("The post is gone".to_string(), false));
                        }
                        Ok(Err(error)) => break 'send Err((format!("{error:#}"), false)),
                        Err(error) => {
                            break 'send Err((format!("Post reader panicked: {error}"), false));
                        }
                    };
                    // Persian then English, like the welcome: each version a
                    // block of its own ending in a link to it in the channel.
                    let mut sentences = String::new();
                    let mut html = String::from("<!doctype html>\n<html><body>\n");
                    let mut text = String::new();
                    for (language, version, direction, view) in [
                        ("fa", &found.fa, "rtl", "مشاهده در کانال"),
                        ("en", &found.en, "ltr", "View in channel"),
                    ] {
                        let Some(version) = version else {
                            continue;
                        };
                        if !version.sentence.is_empty() {
                            if !sentences.is_empty() {
                                sentences.push_str(" / ");
                            }
                            sentences.push_str(&version.sentence);
                        }
                        if !text.is_empty() {
                            html.push_str("<hr>\n");
                            text.push_str("\n\n————————\n\n");
                        }
                        let link = format!("{}/p/{post}?l={language}", app.url);
                        // Post links and images are root-relative; a mail
                        // client needs them whole.
                        let _ = write!(
                            html,
                            "<div dir=\"{direction}\" lang=\"{language}\">\n{}<p><a href=\"{link}\">{view}</a></p>\n</div>\n",
                            version.html.replace("=\"/", &format!("=\"{}/", app.url))
                        );
                        let _ = write!(text, "{}\n\n{view}: {link}\n", version.text.trim());
                    }
                    html.push_str("</body></html>\n");
                    let subject = if sentences.is_empty() {
                        format!("{EMAIL_TITLE} · {post}")
                    } else {
                        format!("{EMAIL_TITLE} · {sentences}")
                    };
                    (subject, html, text)
                };
                let to = match email.parse::<Mailbox>() {
                    Ok(to) => to,
                    Err(error) => break 'send Err((format!("{email}: {error}"), false)),
                };
                let from = match Address::new(EMAIL_USER, &app.host) {
                    Ok(address) => Mailbox::new(Some(CHANNEL_TITLE.to_string()), address),
                    Err(error) => {
                        break 'send Err((format!("{EMAIL_USER}@{}: {error}", app.host), false));
                    }
                };
                // Gmail refuses mail without one.
                let mut message = match Message::builder()
                    .message_id(Some(format!(
                        "<{post}.{subscriber}.{}@{}>",
                        now(),
                        app.host
                    )))
                    .from(from)
                    .to(to.clone())
                    .subject(subject)
                    .multipart(MultiPart::alternative_plain_html(text, html))
                {
                    Ok(message) => message,
                    Err(error) => break 'send Err((error.to_string(), false)),
                };
                message.sign(&dkim);

                // The domain's MX hosts, most preferred first. A domain with
                // none takes its mail itself (RFC 5321 §5.1); a single `.`
                // says it takes none (RFC 7505).
                let domain = to.email.domain().to_string();
                let mut hosts = Vec::new();
                match resolver.mx_lookup(format!("{domain}.")).await {
                    Ok(lookup) => {
                        let mut exchanges = Vec::new();
                        for record in lookup.answers() {
                            if let RData::MX(mx) = &record.data {
                                exchanges.push((mx.preference, mx.exchange.to_ascii()));
                            }
                        }
                        exchanges.sort();
                        for (_, exchange) in exchanges {
                            hosts.push(exchange.trim_end_matches('.').to_string());
                        }
                    }
                    Err(error) if error.is_no_records_found() => {}
                    Err(error) => {
                        break 'send Err((
                            format!("Could not look up the MX of {domain}: {error}"),
                            false,
                        ));
                    }
                }
                if hosts.is_empty() {
                    hosts.push(domain.clone());
                }
                if hosts.contains(&String::new()) {
                    break 'send Err((format!("{domain} accepts no email"), false));
                }

                let bytes = message.formatted();
                let mut last = String::new();
                for host in &hosts {
                    // Encrypted whenever the server offers STARTTLS, with any
                    // certificate, as mail servers do among themselves; plain
                    // when it does not.
                    let tls = match TlsParameters::builder(host.clone())
                        .dangerous_accept_invalid_certs(true)
                        .dangerous_accept_invalid_hostnames(true)
                        .build()
                    {
                        Ok(tls) => tls,
                        Err(error) => break 'send Err((format!("{host}: {error}"), false)),
                    };
                    let transport =
                        AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host.as_str())
                            .port(25)
                            .hello_name(ClientId::Domain(app.host.clone()))
                            .tls(Tls::Opportunistic(tls))
                            .timeout(Some(Duration::from_secs(60)))
                            .build();
                    match transport.send_raw(message.envelope(), &bytes).await {
                        Ok(_) => break 'send Ok(host.clone()),
                        // The domain answered; its other hosts would say the same.
                        Err(error) if error.is_transient() => {
                            break 'send Err((format!("{host}: {error}"), true));
                        }
                        Err(error) if error.is_permanent() => {
                            break 'send Err((format!("{host}: {error}"), false));
                        }
                        // This host could not be reached; the next one might be.
                        Err(error) => {
                            tracing::warn!(msg = "Could not reach mail host, trying the next", kind = kind, post = post, email = %email, host = %host, error = %error);
                            last = format!("{host}: {error}");
                        }
                    }
                }
                Err((last, false))
            };

            let at = now();
            let (status, next_at, error) = match outcome {
                Ok(host) => {
                    tracing::info!(msg = "Sent email", kind = kind, post = post, email = %email, host = %host);
                    ("sent", None, None)
                }
                Err((error, true)) if attempts < 3 => {
                    let minutes = [5, 30, 120][attempts as usize];
                    tracing::warn!(msg = "Deferred email", kind = kind, post = post, email = %email, minutes = minutes, error = %error);
                    ("pending", Some(at + minutes * 60_000), Some(error))
                }
                Err((error, _)) => {
                    tracing::error!(msg = "Could not send email", kind = kind, post = post, email = %email, error = %error);
                    ("failed", None, Some(error))
                }
            };
            let recorded = database(&app, move |connection| {
                connection.execute(
                    "UPDATE delivery
                     SET status = ?1, attempts = attempts + 1, next_at = ?2, error = ?3, at = ?4
                     WHERE post = ?5 AND subscriber = ?6",
                    params![status, next_at, error, at, post, subscriber],
                )
            })
            .await;
            // Going on could send the same email again and again.
            if let Err(error) = recorded {
                tracing::error!(msg = "Could not record email, stopped sending", kind = kind, post = post, email = %email, error = %format!("{error:#}"));
                return;
            }
        }
    });

    let router = Router::new()
        .route("/", get(page))
        .route("/index.html", get(page))
        .route("/p/{post}", get(page))
        .route("/posts/{post}/{file}", get(media))
        .route("/api/visit", post(visit))
        .route("/api/posts", get(posts))
        .route("/api/views", post(views))
        .route("/api/captcha", get(captcha))
        .route("/api/subscribe", post(subscribe))
        .route("/rss.xml", get(rss))
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
/// tags filled in, in the language of `?l=`, Persian without one. The feed
/// itself is drawn by the front end from `/api/posts`; `{{post}}` tells it
/// which post to open at.
async fn page(
    State(app): State<Arc<App>>,
    post: Option<Path<u32>>,
    Query(query): Query<LanguageQuery>,
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
    let mut language = match query.l.as_deref() {
        Some("en") => "en",
        _ => "fa",
    };
    let mut title = CHANNEL_TITLE.to_string();
    let mut description = match language {
        "en" => CHANNEL_DESCRIPTION_EN.to_string(),
        _ => CHANNEL_DESCRIPTION_FA.to_string(),
    };
    let mut kind = "website";
    let mut published = String::new();
    let mut focus = String::new();
    let mut path = String::new();
    if let Some(Path(id)) = post {
        path = format!("p/{id}");
        let directory = app.posts.clone();
        match tokio::task::spawn_blocking(move || read_post(&directory, id)).await {
            Ok(Ok(Some(found))) => {
                if let Some((picked, version)) = found.pick(language) {
                    language = picked;
                    // A post of pictures alone has no sentence to show.
                    title = if version.sentence.is_empty() {
                        format!("{CHANNEL_TITLE} · {id}")
                    } else {
                        version.sentence.clone()
                    };
                    description = version.excerpt.clone();
                }
                kind = "article";
                published = format!("{}:00{TIMEZONE}", found.date);
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

    let url = format!("{}/{path}?l={language}", app.url);
    let locale = if language == "en" { "en_US" } else { "fa_IR" };

    let values = [
        ("channel_title", CHANNEL_TITLE),
        ("channel_description_fa", CHANNEL_DESCRIPTION_FA),
        ("channel_description_en", CHANNEL_DESCRIPTION_EN),
        ("contact", CHANNEL_CONTACT),
        (
            "contact_text",
            CHANNEL_CONTACT.trim_start_matches("https://"),
        ),
        ("channel_avatar", CHANNEL_AVATAR),
        ("telegram", CHANNEL_TELEGRAM),
        ("github", CHANNEL_GITHUB),
        ("title", title.as_str()),
        ("description", description.as_str()),
        ("type", kind),
        ("published", published.as_str()),
        ("url", url.as_str()),
        ("language", language),
        ("locale", locale),
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
            Some(value) => escape(value, &mut html),
            None => html.push_str(&rest[start..start + 2 + end + 2]),
        }
        rest = &after[end + 2..];
    }
    html.push_str(rest);
    (status, Html(html)).into_response()
}

/// `GET /posts/<n>/<file>`: a file from a post's folder. `fa.md`, `en.md` and
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
        || file == "fa.md"
        || file == "en.md"
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
    /// Every email subscribed, the channel's subscriber count.
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

    let ip = client_ip(&headers, socket);

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
            transaction.query_row("SELECT count(*) FROM subscriber", [], |row| row.get(0))?;
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
    /// Every post in the channel, for the channel info's post count.
    total: usize,
}

/// `GET /api/posts`: one page of posts, newest page when no cursor is given.
/// The directory is listed and the posts read on every call.
async fn posts(State(app): State<Arc<App>>, Query(query): Query<PostsQuery>) -> Response {
    let directory = app.posts.clone();
    let read = tokio::task::spawn_blocking(move || -> anyhow::Result<PostsResponse> {
        let ids = post_ids(&directory)?;

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
            total: ids.len(),
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
        let folder = app.posts.join(id.to_string());
        if !ids.contains(&id) && (folder.join("fa.md").is_file() || folder.join("en.md").is_file())
        {
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

#[derive(Serialize)]
struct CaptchaResponse {
    /// What `POST /api/subscribe` names this captcha by.
    id: String,
    /// A `data:image/png;base64,…` URL of the picture.
    image: String,
}

/// `GET /api/captcha`: a new picture of characters for the Join panel. One per
/// client address per second, for at most 100 addresses in that second; the
/// rest get a `429` and come back a second later.
async fn captcha(
    State(app): State<Arc<App>>,
    ConnectInfo(socket): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if robot(&app, &headers, "captcha").await {
        return StatusCode::NO_CONTENT.into_response();
    }
    let ip = client_ip(&headers, socket);
    let mut allowed = true;
    {
        let mut recent = match app.recent.lock() {
            Ok(recent) => recent,
            Err(poisoned) => poisoned.into_inner(),
        };
        let started = Instant::now();
        recent.retain(|(_, at)| started.duration_since(*at) < Duration::from_secs(1));
        for (seen, _) in recent.iter() {
            if *seen == ip {
                allowed = false;
            }
        }
        if recent.len() >= 100 {
            allowed = false;
        }
        if allowed {
            recent.push((ip, started));
        }
    }
    if !allowed {
        return (StatusCode::TOO_MANY_REQUESTS, [(header::RETRY_AFTER, "1")]).into_response();
    }

    let made = tokio::task::spawn_blocking(|| {
        let made = captcha::by_name(captcha::Difficulty::Easy, captcha::CaptchaName::Lucy);
        (made.chars_as_string(), made.as_base64())
    })
    .await;
    let (answer, image) = match made {
        Ok((answer, Some(image))) => (answer, image),
        Ok((_, None)) => {
            tracing::error!(msg = "Could not draw a captcha");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        Err(error) => {
            tracing::error!(msg = "Captcha maker panicked", error = %error);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let id = match Ulid::new() {
        Ok(id) => id.to_string(),
        Err(error) => {
            tracing::error!(msg = "Could not make a captcha id", error = %error);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    tracing::trace!(msg = "Made captcha", id = %id, ip = %ip);
    {
        let mut captchas = match app.captchas.lock() {
            Ok(captchas) => captchas,
            Err(poisoned) => poisoned.into_inner(),
        };
        let made = Instant::now();
        captchas.retain(|(_, _, at)| made.duration_since(*at) < Duration::from_secs(600));
        if captchas.len() >= 1000 {
            captchas.pop_front();
        }
        captchas.push_back((id.clone(), answer, made));
    }
    Json(CaptchaResponse {
        id,
        image: format!("data:image/png;base64,{image}"),
    })
    .into_response()
}

#[derive(Deserialize)]
struct SubscribeRequest {
    email: String,
    /// The id from `GET /api/captcha`.
    captcha: String,
    /// What the reader read in its picture.
    answer: String,
}

#[derive(Serialize)]
struct SubscribeError {
    /// `email` or `captcha`: which field to fix.
    error: &'static str,
}

/// `POST /api/subscribe`: an email and a solved captcha. `204` whether the
/// email is new or already subscribed, so nobody can learn who is. A new
/// subscriber is sent a welcome at once, and every post out at this moment is
/// marked `skipped` for them; they are sent the ones that come after. A known
/// email gets nothing, so the form cannot flood anyone's inbox.
async fn subscribe(
    State(app): State<Arc<App>>,
    ConnectInfo(socket): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<SubscribeRequest>,
) -> Response {
    if robot(&app, &headers, "subscribe").await {
        return StatusCode::NO_CONTENT.into_response();
    }
    // Checked before the captcha, which a typo should not cost.
    let email = body.email.trim().to_string();
    if email.len() > 254 || email.parse::<Address>().is_err() {
        return (
            StatusCode::BAD_REQUEST,
            Json(SubscribeError { error: "email" }),
        )
            .into_response();
    }
    let mut solved = false;
    {
        let mut captchas = match app.captchas.lock() {
            Ok(captchas) => captchas,
            Err(poisoned) => poisoned.into_inner(),
        };
        let checked = Instant::now();
        captchas.retain(|(_, _, at)| checked.duration_since(*at) < Duration::from_secs(600));
        let mut found = None;
        for (index, (id, _, _)) in captchas.iter().enumerate() {
            if *id == body.captcha {
                found = Some(index);
            }
        }
        // One try: right or wrong, it is gone.
        if let Some(index) = found
            && let Some((_, answer, _)) = captchas.remove(index)
        {
            solved = answer.eq_ignore_ascii_case(body.answer.trim());
        }
    }
    if !solved {
        return (
            StatusCode::BAD_REQUEST,
            Json(SubscribeError { error: "captcha" }),
        )
            .into_response();
    }

    let ip = client_ip(&headers, socket).to_string();
    let directory = app.posts.clone();
    let ids = match tokio::task::spawn_blocking(move || post_ids(&directory)).await {
        Ok(Ok(ids)) => ids,
        Ok(Err(error)) => {
            tracing::error!(msg = "Could not list posts", error = %format!("{error:#}"));
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        Err(error) => {
            tracing::error!(msg = "Posts reader panicked", error = %error);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let logged = email.clone();
    // Accepted and counted like anyone, but sent nothing, ever.
    let quiet = noreply(&email);
    let result = database(&app, move |connection| {
        let transaction = connection.unchecked_transaction()?;
        let at = now();
        let added = transaction.execute(
            "INSERT INTO subscriber (email, ip, at) VALUES (?1, ?2, ?3)
             ON CONFLICT (email) DO NOTHING",
            params![email, ip, at],
        )?;
        if added == 1 {
            let subscriber = transaction.last_insert_rowid();
            // Their welcome, sent at once; post numbers are never negative.
            if quiet {
                transaction.execute(
                    "INSERT INTO delivery (post, subscriber, status, attempts, next_at, error, at)
                     VALUES (-1, ?1, 'skipped', 0, NULL, 'noreply address', ?2)",
                    params![subscriber, at],
                )?;
            } else {
                transaction.execute(
                    "INSERT INTO delivery (post, subscriber, status, attempts, next_at, error, at)
                     VALUES (-1, ?1, 'pending', 0, ?2, NULL, ?2)",
                    params![subscriber, at],
                )?;
            }
            for id in &ids {
                transaction.execute(
                    "INSERT INTO delivery (post, subscriber, status, attempts, next_at, error, at)
                     VALUES (?1, ?2, 'skipped', 0, NULL, NULL, ?3)",
                    params![id, subscriber, at],
                )?;
            }
        }
        transaction.commit()?;
        Ok(added == 1)
    })
    .await;
    match result {
        Ok(added) => {
            tracing::info!(msg = "Subscribed", email = %logged, new = added);
            if added && quiet {
                tracing::warn!(msg = "Skipped welcome email to a noreply address", email = %logged);
            } else if added {
                app.wake.notify_one();
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Err(error) => {
            tracing::error!(msg = "Could not subscribe", email = %logged, error = %format!("{error:#}"));
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `GET /rss.xml`: the newest [`PAGE_SIZE`] posts, newest first, each whole as
/// HTML with absolute links. One feed per language, by `?l=`, Persian without
/// one; a post missing that language comes in the other. Feed readers are not
/// counted as robots here.
async fn rss(State(app): State<Arc<App>>, Query(query): Query<LanguageQuery>) -> Response {
    let language = match query.l.as_deref() {
        Some("en") => "en",
        _ => "fa",
    };
    let directory = app.posts.clone();
    let read = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Post>> {
        let ids = post_ids(&directory)?;
        let mut posts = Vec::new();
        for id in ids[ids.len().saturating_sub(PAGE_SIZE)..].iter().rev() {
            match read_post(&directory, *id) {
                Ok(Some(post)) => posts.push(post),
                Ok(None) => {}
                Err(error) => {
                    tracing::error!(msg = "Skipped unreadable post", post = id, error = %format!("{error:#}"));
                }
            }
        }
        Ok(posts)
    })
    .await;
    let posts = match read {
        Ok(Ok(posts)) => posts,
        Ok(Err(error)) => {
            tracing::error!(msg = "Could not read posts", error = %format!("{error:#}"));
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        Err(error) => {
            tracing::error!(msg = "Posts reader panicked", error = %error);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let url = &app.url;
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str(
        "<rss version=\"2.0\" xmlns:atom=\"http://www.w3.org/2005/Atom\">\n<channel>\n<title>",
    );
    escape(CHANNEL_TITLE, &mut xml);
    xml.push_str("</title>\n<description>");
    escape(
        match language {
            "en" => CHANNEL_DESCRIPTION_EN,
            _ => CHANNEL_DESCRIPTION_FA,
        },
        &mut xml,
    );
    xml.push_str("</description>\n");
    let _ = writeln!(xml, "<language>{language}</language>");
    let _ = writeln!(xml, "<link>{url}/?l={language}</link>");
    let _ = writeln!(
        xml,
        "<atom:link href=\"{url}/rss.xml?l={language}\" rel=\"self\" type=\"application/rss+xml\"/>"
    );
    let months = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    for post in posts {
        let Some((_, version)) = post.pick(language) else {
            continue;
        };
        xml.push_str("<item>\n<title>");
        if version.sentence.is_empty() {
            escape(&format!("{CHANNEL_TITLE} · {}", post.id), &mut xml);
        } else {
            escape(&version.sentence, &mut xml);
        }
        xml.push_str("</title>\n");
        let _ = writeln!(xml, "<link>{url}/p/{}?l={language}</link>", post.id);
        // Without `?l=`, as before there were feeds per language, so a reader
        // that has the items already does not show them again.
        let _ = writeln!(xml, "<guid isPermaLink=\"true\">{url}/p/{}</guid>", post.id);
        // RFC 822, from the `YYYY-MM-DDTHH:MM` read_post already checked.
        if let Ok(month) = post.date[5..7].parse::<usize>()
            && (1..=12).contains(&month)
        {
            let _ = writeln!(
                xml,
                "<pubDate>{} {} {} {}:00 {}</pubDate>",
                &post.date[8..10],
                months[month - 1],
                &post.date[..4],
                &post.date[11..16],
                TIMEZONE.replace(':', "")
            );
        }
        xml.push_str("<description>");
        escape(
            &version.html.replace("=\"/", &format!("=\"{url}/")),
            &mut xml,
        );
        xml.push_str("</description>\n</item>\n");
    }
    xml.push_str("</channel>\n</rss>\n");
    (
        [(header::CONTENT_TYPE, "application/rss+xml; charset=utf-8")],
        xml,
    )
        .into_response()
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

/// Read and render `<posts>/<id>/fa.md` and `<posts>/<id>/en.md`. `None` when
/// the folder holds neither.
///
/// Each file opens with a header and then Markdown, and when both are there
/// their dates must agree:
///
/// ```text
/// ---
/// date: 2026-10-19 14:30
/// ---
/// Hello, this is the post.
/// ![](cat.jpg)
/// ```
fn read_post(directory: &FilePath, id: u32) -> anyhow::Result<Option<Post>> {
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
    // One space between words, cut at `limit` characters with an ellipsis.
    let squeeze = |text: &str, limit: usize| -> String {
        let mut words = String::new();
        for word in text.split_whitespace() {
            if !words.is_empty() {
                words.push(' ');
            }
            words.push_str(word);
        }
        match words.char_indices().nth(limit) {
            Some((cut, _)) => format!("{}…", &words[..cut]),
            None => words,
        }
    };

    // The date and the file it came from, to name both when they disagree.
    let mut dated: Option<(String, PathBuf)> = None;
    let mut fa = None;
    let mut en = None;
    for language in ["fa", "en"] {
        let path = directory
            .join(id.to_string())
            .join(format!("{language}.md"));
        let source = match std::fs::read_to_string(&path) {
            Ok(source) => source.replace("\r\n", "\n"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
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
        // `YYYY-MM-DD HH:MM`, checked by position rather than parsed into a
        // type: the front end only ever reads it back as text.
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
            valid =
                (1..=12).contains(&month) && (1..=31).contains(&day) && hour < 24 && minute < 60;
        }
        if !valid {
            bail!(
                "{}: date `{date}` is not `YYYY-MM-DD HH:MM`",
                path.display()
            );
        }
        let date = format!("{}T{}", &date[..10], &date[11..]);
        match &dated {
            Some((first, first_path)) if *first != date => {
                bail!(
                    "{} is dated {} but {} is dated {}; they must agree",
                    first_path.display(),
                    first.replacen('T', " ", 1),
                    path.display(),
                    date.replacen('T', " ", 1)
                );
            }
            Some(_) => {}
            None => dated = Some((date, path)),
        }

        // Plain text with a `\n` at every line break and block end, so the
        // first sentence can stop at the first line.
        let mut excerpt = String::new();
        let mut events = Vec::new();
        let parser = pulldown_cmark::Parser::new_ext(
            body,
            Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS,
        );
        for event in parser {
            let event = match event {
                // Written like a Telegram message: Enter is a new line.
                Event::SoftBreak => {
                    excerpt.push('\n');
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
                    excerpt.push('\n');
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
                }) => Event::Start(Tag::Image {
                    link_type,
                    dest_url: local(dest_url.into_static()),
                    title,
                    id: reference,
                }),
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

        // The first sentence ends at the first `.`, `!`, `?` or `؟` followed
        // by a space, or at the first line break, whichever comes first.
        let excerpt = excerpt.trim_start();
        let mut split = excerpt.len();
        let mut previous = None;
        for (index, character) in excerpt.char_indices() {
            if character == '\n'
                || (character.is_whitespace() && matches!(previous, Some('.' | '!' | '?' | '؟')))
            {
                split = index;
                break;
            }
            previous = Some(character);
        }
        let (sentence, rest) = excerpt.split_at(split);

        let version = Version {
            html,
            sentence: squeeze(sentence, 100),
            excerpt: squeeze(rest, 200),
            text: excerpt.to_string(),
        };
        if language == "fa" {
            fa = Some(version);
        } else {
            en = Some(version);
        }
    }

    let Some((date, _)) = dated else {
        return Ok(None);
    };
    Ok(Some(Post {
        id,
        date,
        fa,
        en,
        views: 0,
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

/// The numbers of the folders under `directory` that hold a `fa.md` or an
/// `en.md`, in order.
fn post_ids(directory: &FilePath) -> anyhow::Result<Vec<u32>> {
    let mut ids = Vec::new();
    let entries = std::fs::read_dir(directory)
        .with_context(|| format!("Could not list {}", directory.display()))?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if let Ok(id) = name.parse::<u32>()
            && (entry.path().join("fa.md").is_file() || entry.path().join("en.md").is_file())
        {
            ids.push(id);
        }
    }
    ids.sort_unstable();
    Ok(ids)
}

/// Whether the user part of `email` says nobody reads it: `noreply` or
/// `no-reply` anywhere in it, in any case. Such subscribers are kept and
/// counted, and sent nothing.
fn noreply(email: &str) -> bool {
    let user = match email.rsplit_once('@') {
        Some((user, _)) => user.to_lowercase(),
        None => email.to_lowercase(),
    };
    user.contains("noreply") || user.contains("no-reply")
}

/// The visitor's address behind our proxy, else the socket's.
fn client_ip(headers: &HeaderMap, socket: SocketAddr) -> IpAddr {
    // The right-most entry of each header is the one our own proxy appended,
    // which the client cannot forge; anything to its left can be.
    let mut ip = None;
    if let Some(forwarded) = header_text(headers, "forwarded")
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
        && let Some(value) = header_text(headers, "x-forwarded-for")
        && let Some(last) = value.rsplit(',').next()
        && let Ok(parsed) = last.trim().parse::<IpAddr>()
    {
        ip = Some(parsed);
    }
    if ip.is_none()
        && let Some(value) = header_text(headers, "x-real-ip")
        && let Ok(parsed) = value.trim().parse::<IpAddr>()
    {
        ip = Some(parsed);
    }
    match ip {
        Some(ip) => ip,
        None => socket.ip(),
    }
}

/// Append `text` to `into`, safe inside HTML or XML text and attributes.
fn escape(text: &str, into: &mut String) {
    for character in text.chars() {
        match character {
            '&' => into.push_str("&amp;"),
            '<' => into.push_str("&lt;"),
            '>' => into.push_str("&gt;"),
            '"' => into.push_str("&quot;"),
            '\'' => into.push_str("&#39;"),
            other => into.push(other),
        }
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
