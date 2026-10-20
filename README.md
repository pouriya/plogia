# plogia

[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**A blog that looks and reads like a Telegram channel.** See it live at [plogia.ir](https://plogia.ir).

Posts are folders of Markdown. plogia serves them as a channel feed, counts who read what, and emails each new post to its subscribers. It is one binary and one SQLite file, with no mail relay, no CDN and nothing else to run beside it.

- **Write like Telegram.** Enter is a new line, a blank line is a new paragraph, and Persian and English lines each read the right way.
- **Readers join three ways:** by email, on the Telegram channel, or with RSS at `/rss.xml`.
- **Views are real.** A view is a post at least half on screen for two seconds, and robots are counted apart.
- **English or Persian.** The page is in English, and readers can switch to Persian, right to left with the solar Hijri calendar.

## Run it

With Docker. `www/` and `posts/` are baked into the image, and the database lives in the `plogia` volume:

```bash
make docker
docker run -d --name plogia -p 8080:8080 -v plogia:/var/lib/plogia plogia:latest --url https://example.com
```

From source, with a Rust toolchain. `make release` leaves the binary in `build/`:

```bash
make release
build/plogia-<version>-<target> --www www --posts posts --db plogia.db --port 8080 --url https://example.com
```

Every flag is required. `--url` is the address readers use: link previews, the RSS feed and emails point there, and its host is the domain emails are sent from.

plogia speaks plain HTTP. Put it behind nginx, Caddy or a CDN that terminates TLS.

## Write a post

One folder per post, named by its number. The number is its order and its link, `/p/12`:

```
posts/
  12/
    post.md
    cat.jpg
```

```markdown
---
date: 2026-10-19 14:30
---
The first line is the title of the link preview.
A single Enter is a line break, like in Telegram.

![](cat.jpg)
```

`date` is your own wall clock, with no zone. A post is live as soon as its folder exists. Subscribers are emailed new posts the next time plogia starts. With Docker, that means building and running a new image.

## Make it yours

What the channel *is* lives as constants at the top of `src/main.rs`: its title, description, avatar, Telegram and GitHub links, the time zone your dates are in, and the email subject prefix. Change them and rebuild. The look is `www/`, served as-is.

## Email

plogia delivers mail itself, straight to each subscriber's mail server. A new subscriber gets a welcome right away; after that, every new post. Gmail, Outlook and the rest refuse mail they cannot verify, so it needs a few things from you first.

<details>
<summary><b>Set up email</b> — outbound port 25, three DNS records and reverse DNS</summary>

**1. Outbound port 25.** Many hosts block it. Check from where plogia runs:

```bash
nc -zv -w5 gmail-smtp-in.l.google.com 25
```

**2. Three DNS records** for the host in `--url`, all of type TXT:

| Name | Value |
|---|---|
| `plogia._domainkey` | the `value` from the `Publish this DKIM record` log line |
| `@` | `v=spf1 ip4:<your server's outgoing IP> -all` |
| `_dmarc` | `v=DMARC1; p=none` |

The DKIM key is generated on first start and kept in the database. Every start logs the record to publish:

```bash
docker logs plogia 2>&1 | grep "Publish this DKIM record"
```

The outgoing IP is not always the one your domain points at, especially behind a CDN or a container platform. Ask from the server itself:

```bash
wget -qO- https://ifconfig.me
```

**3. Reverse DNS.** The outgoing IP should resolve back to your domain, and Gmail refuses mail without it (`5.7.25`). That is set by whoever owns the IP, not in your DNS panel, so ask your host.

Check the records with `dig +short TXT plogia._domainkey.example.com`. Then subscribe your own address and open the welcome in Gmail with *Show original*. SPF, DKIM and DMARC should all say `PASS`.

</details>

<details>
<summary><b>When an email does not arrive</b></summary>

Every email logs one line: `Sent email`, `Deferred email` or `Could not send email`. Failed sends quote the receiving server's own reason:

```bash
docker logs -f plogia 2>&1 | grep -E 'Subscribed|email|mail host'
```

The same is kept per subscriber and post, and the image ships the `sqlite3` shell:

```bash
docker exec -it plogia sqlite3 /var/lib/plogia/plogia.db "select * from delivery"
```

Post `-1` is the welcome. A `4xx` answer is retried after 5 minutes, 30 minutes and 2 hours; anything else is final. Once the cause is fixed, send failed ones again and restart:

```bash
docker exec -it plogia sqlite3 /var/lib/plogia/plogia.db \
  "update delivery set status='pending', attempts=0, next_at=0, error=NULL where status='failed'"
docker restart plogia
```

</details>

Subscribing needs only an email and a captcha. There is no confirmation and no unsubscribe yet. An address with `noreply` or `no-reply` before the `@` is accepted and counted, but never sent anything.

## Development

```bash
make start-dev   # serves www/ and posts/ on :8080 with debug logs
make all         # build, clippy, tests and format check: run before every commit
```

`AGENTS.md` describes everything else: the routes, the tables, the logs and the conventions.
