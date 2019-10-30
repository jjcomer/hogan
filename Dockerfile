# We need to use the Rust build image, because
# we need the Rust compile and Cargo tooling
FROM rust:1 as build

# Install cmake as it is not included in muslrust, but is needed by libssh2-sys
RUN apt-get update && apt-get install -y \
  cmake clang llvm build-essential curl llvm-dev libclang-dev libsnappy-dev liblz4-dev libzstd-dev libgflags-dev zlib1g-dev libbz2-dev\
  --no-install-recommends && \
  rm -rf /var/lib/apt/lists/*

# RUN apk --update add \
#   clang llvm build-base && \
#   ln -s /usr/bin/g++ /usr/bin/musl-g++ 

WORKDIR /app
# Creates a dummy project used to grab dependencies
RUN USER=root cargo init --bin

# Copies over *only* your manifests
COPY ./Cargo.* ./

# Builds your dependencies and removes the
# fake source code from the dummy project
RUN cargo build --release
RUN rm src/*.rs
RUN rm target/release/hogan

# Copies only your actual source code to
# avoid invalidating the cache at all
COPY ./src ./src

# Builds again, this time it'll just be
# your actual source files being built
RUN cargo build --release

# FROM alpine:latest as certs
# RUN apk --update add ca-certificates

# Create a new stage with a minimal image
# because we already have a binary built
FROM debian:buster-slim

RUN apt-get update && apt-get install -y \
  libssl-dev \
  --no-install-recommends && \
  rm -rf /var/lib/apt/lists/*

# Copies standard SSL certs from the "build" stage
#COPY --from=certs /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt

# Copies the binary from the "build" stage
COPY --from=build /app/target/release/hogan /bin/

# Configures the startup!
ENTRYPOINT ["/bin/hogan"]
