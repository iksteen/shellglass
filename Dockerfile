# From-source image for local use (`docker build .`). Reports version 0.0.0-dev (the
# tag-stamped version is only applied in CI). The published multi-arch image is built
# from pre-compiled release binaries instead — see ./Dockerfile.release.
#
# Build: aws-lc-rs (via rustls-acme) needs cmake + a C compiler.
FROM rust:1-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends cmake && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --release --locked

# Runtime: slim glibc image; ca-certificates so the hub can validate Let's Encrypt.
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -r -u 10001 shellglass \
    && mkdir /data && chown shellglass /data
COPY --from=build /src/target/release/shellglass /usr/local/bin/shellglass

# ACME cache lives here — mount a named volume at /data to keep the account +
# certificate across restarts (pass --acme-cache /data/acme).
VOLUME /data
USER shellglass
EXPOSE 443
ENTRYPOINT ["shellglass"]
# Override with your own flags, e.g.:
#   docker run -p 443:443 -v shellglass-acme:/data IMAGE \
#     hub --bind 0.0.0.0:443 --allow <ID> \
#     --acme-domain hub.example.com --acme-email you@example.com \
#     --acme-cache /data/acme --acme-production
CMD ["--help"]
