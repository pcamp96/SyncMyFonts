FROM rust:1-bookworm AS builder
WORKDIR /app
COPY . .
RUN cargo build --release -p syncmyfonts-server

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/syncmyfonts-server /usr/local/bin/syncmyfonts-server
ENV SYNCMYFONTS_LISTEN=0.0.0.0:7368
ENV SYNCMYFONTS_DATA_DIR=/data
VOLUME ["/data"]
EXPOSE 7368
CMD ["syncmyfonts-server"]
