FROM rust:1.94-slim-bookworm AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY vendor ./vendor
COPY media ./media
COPY mobile ./mobile
COPY core ./core
COPY test-server ./test-server
COPY .docker-vendor /cargo-vendor
RUN cargo build --locked --offline --release -p lingxi-webrtc-test-server \
    --config 'source.crates-io.replace-with="local-vendor"' \
    --config 'source.local-vendor.directory="/cargo-vendor"'
FROM debian:bookworm-slim
COPY --from=builder /src/target/release/lingxi-webrtc-test-server /usr/local/bin/
USER 65534:65534
EXPOSE 8080/tcp 50000-50015/udp
ENTRYPOINT ["lingxi-webrtc-test-server"]
