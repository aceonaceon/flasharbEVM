# Build
FROM rust:1.91-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY web ./web
COPY config.toml ./
RUN cargo build --release --locked && strip target/release/flasharb-shadow

# Run: a small image with only the binary and the CA bundle (copied from the build
# image, so no apt step is needed). No keys, no wallet.
FROM debian:bookworm-slim
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
RUN useradd --system --uid 10001 --home /app flasharb
WORKDIR /app
COPY --from=build /src/target/release/flasharb-shadow /usr/local/bin/flasharb-shadow
COPY config.toml /app/config.toml
RUN mkdir -p /app/data && chown flasharb /app/data
USER flasharb
ENTRYPOINT ["flasharb-shadow", "--config", "/app/config.toml"]
CMD ["run"]
