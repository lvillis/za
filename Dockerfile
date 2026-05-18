FROM rust:1.95.0-alpine3.23 AS builder

RUN set -ex \
    && apk add --no-cache build-base musl-dev openssl-dev perl make

WORKDIR /workspace

COPY Cargo.toml /workspace/Cargo.toml
COPY Cargo.lock /workspace/Cargo.lock

RUN mkdir -p /workspace/src && echo "fn main() {}" > /workspace/src/main.rs

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    set -ex \
    && cargo build --release --locked

RUN rm -f /workspace/src/main.rs
COPY src/ /workspace/src/

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    set -ex \
    && cargo build --release --locked


FROM scratch AS runtime

COPY --from=builder /workspace/target/release/za /usr/local/bin/za

ENTRYPOINT ["/usr/local/bin/za"]
