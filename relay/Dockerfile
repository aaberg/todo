# Build the relay binary
FROM rust:slim-trixie AS build
WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY common common
COPY relay relay
COPY cli cli

RUN cargo build --release -p todo-relay

# Runtime image
FROM debian:trixie-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Fixed UID so host bind mounts can be chowned predictably
RUN useradd --uid 10000 --system --home /var/lib/todo-relay --create-home todo-relay

COPY --from=build /build/target/release/todo-relay /usr/local/bin/todo-relay

USER todo-relay
WORKDIR /var/lib/todo-relay
VOLUME /var/lib/todo-relay

ENV TODO_RELAY_BIND=0.0.0.0:3000 \
    TODO_RELAY_DATABASE=/var/lib/todo-relay/relay.db

EXPOSE 3000

ENTRYPOINT ["todo-relay"]
