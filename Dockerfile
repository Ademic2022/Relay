# relay-server: rendezvous (udp/21116) + relay (udp/21117).
FROM rust:1.88-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p relay-server

FROM debian:bookworm-slim
RUN useradd --system --home /data relay && mkdir -p /data && chown relay /data
COPY --from=build /src/target/release/relay-server /usr/local/bin/relay-server
USER relay
VOLUME /data
EXPOSE 21116/udp 21117/udp
ENTRYPOINT ["relay-server", "--data-dir", "/data"]
