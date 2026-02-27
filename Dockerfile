FROM rust:1.93.1-alpine3.23 AS builder

RUN set -ex \
        \
    && apk update \
    && apk upgrade \
    && apk add --update --no-cache build-base musl-dev openssl-dev perl make

WORKDIR /opt/app

COPY Cargo.toml /opt/app/Cargo.toml
COPY Cargo.lock /opt/app/Cargo.lock

RUN mkdir -p /opt/app/src && echo "fn main() {}" > /opt/app/src/main.rs

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    set -ex \
    && cargo build --release --locked

RUN rm -f /opt/app/src/main.rs
COPY src/ /opt/app/src/

RUN set -ex \
        \
    && cargo build --release --locked


FROM scratch AS runtime

COPY --from=builder /opt/app/target/release/za /usr/local/bin/za

ENTRYPOINT ["/usr/local/bin/za"]
