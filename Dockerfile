ARG DOCKER_REGISTRY
ARG DOCKER_RUST_IMAGE=rust:1.98.1-alpine3.24
ARG DOCKER_ALPINE_IMAGE=alpine:3.24.2
FROM ${DOCKER_REGISTRY}${DOCKER_RUST_IMAGE} AS builder

# gcc and musl-dev because SQLite is compiled from C by its build script; make
# because the build goes through the Makefile like every other one.
RUN apk add --no-cache make gcc musl-dev

WORKDIR /plogia

COPY Cargo.toml Cargo.lock Makefile ./
COPY src src
RUN make release && mv build/plogia-* build/plogia


FROM ${DOCKER_REGISTRY}${DOCKER_ALPINE_IMAGE}
ARG PLOGIA_VERSION
LABEL "org.opencontainers.image.authors"="pouriya.jahanbakhsh@gmail.com"
LABEL "org.opencontainers.image.title"="plogia"
LABEL "org.opencontainers.image.description"="A blog that looks and reads like a Telegram channel, with view counts"
LABEL "org.opencontainers.image.version"="${PLOGIA_VERSION}"

# The sqlite3 shell, to look into the database from inside the container:
# `docker exec -it <container> sqlite3 /var/lib/plogia/plogia.db`.
RUN apk add --no-cache sqlite

# The front end and the posts are baked in: publishing a post is building a
# new image. The database is the only state, and lives in the volume.
COPY --from=builder /plogia/build/plogia /usr/local/bin/plogia
COPY www /usr/share/plogia/www
COPY posts /usr/share/plogia/posts
VOLUME /var/lib/plogia
EXPOSE 8080

ENTRYPOINT ["plogia", \
	"--www", "/usr/share/plogia/www", \
	"--posts", "/usr/share/plogia/posts", \
	"--db", "/var/lib/plogia/plogia.db", \
	"--port", "8080"]
