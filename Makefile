TARGET ?= $(shell rustc -vV 2>/dev/null | awk '$$1 == "host:"{print $$2}')
BUILD_DIR=$(CURDIR)/build
CARGO_TARGET_DIR ?= $(CURDIR)/target
VERSION=$(shell cat Cargo.toml | awk 'BEGIN{FS="[ \"]"}$$1 == "version"{print $$4;exit}')
CMD=${BUILD_DIR}/plogia-${VERSION}-${TARGET}
DEV_CMD=${BUILD_DIR}/plogia-${VERSION}-${TARGET}-dev

DIST_DIR=${BUILD_DIR}/dist
DIST_NAME=plogia-${VERSION}-${TARGET}

# What `make start-dev` serves, and where it keeps its database.
WWW_DIR ?= $(CURDIR)/www
POSTS_DIR ?= $(CURDIR)/posts
DB_FILE ?= $(CURDIR)/plogia.db
PORT ?= 8080
# What links in previews, the RSS feed and emails point at; its host signs and
# sends the emails.
URL ?= http://localhost:${PORT}

# `make docker`. DOCKER_REGISTRY prefixes both base images, with its trailing
# slash, for a machine that cannot reach Docker Hub; empty means whatever the
# local daemon resolves (a cached image counts). Both base images are pinned to
# exact versions, so a build reuses what is cached instead of pulling a moved
# tag; bump them here on purpose, not by accident.
DOCKER_REGISTRY ?=
DOCKER_RUST_IMAGE ?= rust:1.98.1-alpine3.24
DOCKER_ALPINE_IMAGE ?= alpine:3.24.2
DOCKER_IMAGE ?= plogia
DOCKER_IMAGE_VERSION ?= ${VERSION}


all: dev clippy test check-style


release: ${BUILD_DIR}
	cargo build --release --target ${TARGET}
	@ cp ${CARGO_TARGET_DIR}/${TARGET}/release/plogia ${CMD}
	@ ls -sh ${BUILD_DIR}/plogia-*


dev: ${BUILD_DIR}
	cargo build --target ${TARGET}
	@ cp ${CARGO_TARGET_DIR}/${TARGET}/debug/plogia ${DEV_CMD}
	@ ls -sh ${BUILD_DIR}/plogia-*dev*


start-dev: dev
	${DEV_CMD} --debug --www ${WWW_DIR} --posts ${POSTS_DIR} --db ${DB_FILE} --port ${PORT} --url ${URL}


# The release binary and www/ in ${DIST_DIR}/${DIST_NAME}.tar.gz, plus its
# .sha256. Posts and the database are the operator's: --posts and --db.
dist:
	cargo build --release --target ${TARGET}
	@ rm -rf ${DIST_DIR}/${DIST_NAME}
	@ mkdir -p ${DIST_DIR}/${DIST_NAME}
	@ cp ${CARGO_TARGET_DIR}/${TARGET}/release/plogia ${DIST_DIR}/${DIST_NAME}/
	@ cp -r www ${DIST_DIR}/${DIST_NAME}/
	@ cd ${DIST_DIR} && COPYFILE_DISABLE=1 tar czf ${DIST_NAME}.tar.gz ${DIST_NAME}
	@ cd ${DIST_DIR} && { command -v sha256sum >/dev/null && sha256sum ${DIST_NAME}.tar.gz \
		|| shasum -a 256 ${DIST_NAME}.tar.gz ; } > ${DIST_NAME}.tar.gz.sha256
	@ echo ${DIST_DIR}/${DIST_NAME}.tar.gz


# Alpine image ${DOCKER_IMAGE}:<version> and :latest, with www/ and posts/
# baked in. Builds inside Docker with `make release`. Pushing is not here.
docker:
	docker build --build-arg DOCKER_REGISTRY=${DOCKER_REGISTRY} \
		--build-arg DOCKER_RUST_IMAGE=${DOCKER_RUST_IMAGE} \
		--build-arg DOCKER_ALPINE_IMAGE=${DOCKER_ALPINE_IMAGE} \
		--build-arg PLOGIA_VERSION=${VERSION} \
		--force-rm -t ${DOCKER_IMAGE}:${DOCKER_IMAGE_VERSION} -t ${DOCKER_IMAGE}:latest .


# The Cargo.toml version, for scripts and the image tag.
version:
	@ echo ${VERSION}


clippy:
	cargo clippy --all-targets --no-deps -- -D warnings


check-style:
	cargo fmt --check --verbose


fmt:
	cargo fmt


lint: clippy check-style


test:
	cargo test --target ${TARGET}


clean:
	@ cargo clean


dist-clean: clean
	@ rm -rf ${BUILD_DIR}


${BUILD_DIR}:
	@ mkdir -p ${BUILD_DIR}


.PHONY: all release dev start-dev dist docker version clippy check-style fmt lint test clean dist-clean
