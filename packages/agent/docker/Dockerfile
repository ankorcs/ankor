# The Ankor game-host agent, one per game host. node-setup.sh's start_agent sets its labels, data
# paths and run command.
# Build from packages/agent, with the version in crates/agent/Cargo.toml:
#   docker build --platform linux/amd64 -f docker/Dockerfile \
#     --build-arg VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' crates/agent/Cargo.toml)" -t ankor-agent:rust .

# ---------------------------------------------------------------------------------------------
FROM rust:1.99-alpine3.24 AS build
# A C toolchain for the bundled SQLite and ring.
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock rustfmt.toml clippy.toml ./
COPY crates crates
# The image's version label must be the version the agent reports, so the build refuses any other.
ARG VERSION
RUN grep -qx "version = \"${VERSION}\"" crates/agent/Cargo.toml \
 || { echo "VERSION must be the version in crates/agent/Cargo.toml, not '${VERSION}'" >&2; exit 1; }
RUN cargo build --release --locked -p ankor-agent --bin ankor-agent \
 && cp target/release/ankor-agent /ankor-agent

# ---------------------------------------------------------------------------------------------
FROM alpine:3.24 AS runtime
# Read by the agent's image sweep: it outlives the tag, which a reload under the same name takes away.
# Do not remove or rename it. An agent refuses to update to an image without this exact label, so a
# release without it can never be installed by an update.
LABEL ankor.image=agent
# The image auto-update compares this version with the registry's tags.
ARG VERSION
LABEL org.opencontainers.image.version="${VERSION}"
LABEL org.opencontainers.image.source="https://github.com/ankorcs/ankor"
LABEL org.opencontainers.image.description="Ankor game-host agent: Runs CS2 game servers of one host for an Ankor controller."
LABEL org.opencontainers.image.licenses="GPL-3.0-only"
LABEL org.opencontainers.image.url="https://ankor.sh"
LABEL org.opencontainers.image.documentation="https://docs.ankor.sh"
# The roots TLS connections are checked against.
RUN apk add --no-cache ca-certificates
COPY --from=build /ankor-agent /usr/local/bin/ankor-agent
# The agent's license, the Apache license of the contract code generated from @ankorcs/protocol, and
# the notices of the crates the binary bundles.
COPY LICENSE LICENSE-APACHE THIRD-PARTY-LICENSES.txt /usr/share/licenses/ankor-agent/

# No USER: the agent reads the socket's gid as root, then drops to uid 1000.
# The event listener is loopback-only; run with --network host.
ENTRYPOINT ["/usr/local/bin/ankor-agent"]
