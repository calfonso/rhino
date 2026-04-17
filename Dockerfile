FROM rust:1.84-bookworm AS builder

WORKDIR /build

# Install protobuf compiler (needed by tonic-build)
RUN apt-get update && apt-get install -y protobuf-compiler && rm -rf /var/lib/apt/lists/*

# Cache dependency builds: copy manifests first, then build deps
COPY Cargo.toml Cargo.lock build.rs ./
COPY proto/ proto/
RUN mkdir -p src/bin && \
    echo 'fn main() {}' > src/bin/server.rs && \
    echo 'pub fn unused() {}' > src/lib.rs && \
    cargo build --release --bin rhino-server 2>/dev/null || true

# Copy actual source and rebuild
COPY src/ src/
RUN cargo build --release --bin rhino-server

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/rhino-server /usr/local/bin/rhino-server

RUN mkdir -p /data/db

EXPOSE 2379

ENTRYPOINT ["rhino-server"]
CMD ["--listen-address", "0.0.0.0:2379", "--db-path", "/data/db/state.db"]
