# The toolchain the repository's checks run in, on a workstation and in CI: Node for the protocol,
# the docs site and the scripts, and the .NET SDK for the plugin. compose.yaml runs it, so a
# contributor needs only Docker. Both are pinned to a minor version, so a check gives the same answer
# on any machine.
# Node keeps the major in .nvmrc, and the SDK is the one the plugin stage of
# packages/plugin/docker/Dockerfile builds and tests with.

FROM node:26.12-trixie-slim AS node

FROM mcr.microsoft.com/dotnet/sdk:10.0
COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules /usr/local/lib/node_modules
RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
 && ln -s ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx \
 && node --version && npm --version && dotnet --version
ENV DOTNET_CLI_TELEMETRY_OPTOUT=1 \
    DOTNET_NOLOGO=1
