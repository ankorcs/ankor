# The toolchain every script in scripts/ runs in, locally and in CI: each script starts this
# container itself (scripts/lib/in-docker.sh), so a contributor needs only Docker. Pinned, so a check
# gives the same answer on any machine. A tool's version, URL and digest change together. It builds
# for the machine's architecture, so on Apple silicon nothing runs emulated.

FROM node:26.12-trixie-slim AS node
FROM docker:29.9-cli AS docker

FROM rust:1.99-trixie
ARG TARGETARCH
RUN rustup component add clippy rustfmt llvm-tools-preview
# .cargo/config.toml links arm64 builds with lld. GNU ld took most of each build, and a mutation run
# relinks every test binary for each mutant. x86_64 already links with rustc's rust-lld.
RUN apt-get update \
 && apt-get install -y --no-install-recommends lld \
 && rm -rf /var/lib/apt/lists/*
# The checks' tools, as their projects' Linux builds, each checked against the digest its GitHub
# release records. cargo-mutants 27.1.0 publishes no arm64 Linux build, so it is compiled there.
# Releases after it carry one (https://github.com/sourcefrog/cargo-mutants/pull/657), which the
# arm64 branch can then download.
RUN set -eu; \
    tool() { \
      curl -fsSL "$1" -o /tmp/tool.tgz; \
      echo "$2  /tmp/tool.tgz" | sha256sum -c -; \
      mkdir /tmp/tool && tar -xzf /tmp/tool.tgz -C /tmp/tool; \
      find /tmp/tool -type f -name "$3" -exec install -m 0755 {} /usr/local/cargo/bin/ \; ; \
      rm -rf /tmp/tool /tmp/tool.tgz; \
    }; \
    case "$TARGETARCH" in \
      amd64) \
        tool https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-0.9.150/cargo-nextest-0.9.150-x86_64-unknown-linux-musl.tar.gz \
          84f0f9ba75d0b6fa117bc418d49747ba78e20e4583057f6fa628f1b17c17234d cargo-nextest; \
        tool https://github.com/EmbarkStudios/cargo-deny/releases/download/0.20.2/cargo-deny-0.20.2-x86_64-unknown-linux-musl.tar.gz \
          9f12ed4c49936e09b48bf862b595cde2fe64fcbd9d74dfacac6131ca824c8d5f cargo-deny; \
        tool https://github.com/rustsec/rustsec/releases/download/cargo-audit%2Fv0.22.2/cargo-audit-x86_64-unknown-linux-musl-v0.22.2.tgz \
          7fb9497f8594b389e5fce5ef9b92db08432996895b2e0c5a0167a69ed445c428 cargo-audit; \
        tool https://github.com/taiki-e/cargo-llvm-cov/releases/download/v0.9.1/cargo-llvm-cov-x86_64-unknown-linux-musl.tar.gz \
          3fca950394a3c49457657c158b1619cec8dfd2647ae5b48746734c0ab969a522 cargo-llvm-cov; \
        tool https://github.com/EmbarkStudios/cargo-about/releases/download/0.9.2/cargo-about-0.9.2-x86_64-unknown-linux-musl.tar.gz \
          9099a59e820c38a68b9d65f300662a567d56562f9a10f6aa4c7e86c17c2566af cargo-about; \
        tool https://github.com/sourcefrog/cargo-mutants/releases/download/v27.1.0/cargo-mutants-x86_64-unknown-linux-gnu.tar.gz \
          dfe6dc37d0342c891d2829b5a695aa57c2d0edecef7e7d0399a30cc6e206411e cargo-mutants ;; \
      arm64) \
        tool https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-0.9.150/cargo-nextest-0.9.150-aarch64-unknown-linux-musl.tar.gz \
          5a7fe62b64a9006e1e47422021df7b1d308cc76c528c9eaf4649a488615ed6fb cargo-nextest; \
        tool https://github.com/EmbarkStudios/cargo-deny/releases/download/0.20.2/cargo-deny-0.20.2-aarch64-unknown-linux-musl.tar.gz \
          995c82be0defc7a025cae49a2aa2644ce8245c9a3318fc4103907c6a285e8c7d cargo-deny; \
        tool https://github.com/rustsec/rustsec/releases/download/cargo-audit%2Fv0.22.2/cargo-audit-aarch64-unknown-linux-gnu-v0.22.2.tgz \
          c6603814ddaa45e51263dafd31c0ac98808f688d26f7395804f9670b0fd599dd cargo-audit; \
        tool https://github.com/taiki-e/cargo-llvm-cov/releases/download/v0.9.1/cargo-llvm-cov-aarch64-unknown-linux-musl.tar.gz \
          49c0085628392a8d727c4464d91ec417c613c5015f5197cd16d5c05aa3fb5ad1 cargo-llvm-cov; \
        tool https://github.com/EmbarkStudios/cargo-about/releases/download/0.9.2/cargo-about-0.9.2-aarch64-unknown-linux-musl.tar.gz \
          af5169282fb6f84e13471493f405437e43ac517744c9ae12fbe2cdf0a6f0e5a8 cargo-about; \
        cargo install --locked cargo-mutants@27.1.0; \
        rm -rf /usr/local/cargo/registry ;; \
      *) echo "no tools for ${TARGETARCH}" >&2; exit 1 ;; \
    esac
# Node writes the protocol corpus from the vendored contract, and protocol-sync.sh builds the contract.
COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules /usr/local/lib/node_modules
# The conformance suite drives its sandbox through the Docker CLI (scripts/conformance.sh).
COPY --from=docker /usr/local/bin/docker /usr/local/bin/docker
RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
 && ln -s ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx \
 && git config --system --add safe.directory /src \
 && chmod -R a+w /usr/local/cargo
# The scripts run as the caller's user, and cargo keeps its locks in CARGO_HOME.
# scripts/lib/in-docker.sh runs a script directly when this is set.
ENV ANKOR_CHECK_CONTAINER=1
WORKDIR /src
