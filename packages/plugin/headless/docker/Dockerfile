# The plugin without CS2: the headless host (ankor-headless) runs the plugin's core on the test
# engine, laid out as an ankor-cs2 image for the agent: the addon overlay it extracts, the labels it
# reads, and an entrypoint for both modes the agent starts the image in. The agent's conformance
# suite runs it in place of CS2. Build from packages/plugin:
#   docker build -f headless/docker/Dockerfile -t ankor-cs2-headless:latest .

FROM mcr.microsoft.com/dotnet/sdk:10.0 AS build
ARG TARGETARCH
WORKDIR /src
COPY . ./
# Self-contained, so the runtime stage is the real image's Debian rather than Microsoft's Ubuntu.
RUN case "$TARGETARCH" in amd64) rid=linux-x64 ;; arm64) rid=linux-arm64 ;; *) echo "no runtime for $TARGETARCH" >&2; exit 1 ;; esac \
 && dotnet publish headless/Ankor.Headless.csproj -c Release -r "$rid" --self-contained -o /out

FROM debian:trixie-slim
# The agent sweeps images by this label and reads the version label to follow releases.
LABEL ankor.image=cs2
LABEL org.opencontainers.image.version=1.0.0
# libicu, as in the real image: the .NET runtime wants real ICU.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates libicu76 \
 && rm -rf /var/lib/apt/lists/* \
 && useradd -u 1000 -U -m -d /home/steam steam \
 && mkdir -p /opt/ankor/overlay/addons/swiftlys2/plugins/Ankor \
             /opt/ankor/overlay/addons/swiftlys2/configs/plugins/Ankor \
             /opt/ankor/overlay/addons/swiftlys2/logs \
             /opt/ankor/overlay/addons/swiftlys2/dumps \
             /opt/ankor/overlay/addons/swiftlys2/data \
             /opt/ankor/overlay/cfg/ankor \
             /home/steam/cs2-dedicated \
             /home/steam/ankor-event-captures \
 && printf 'fake plugin\n' > /opt/ankor/overlay/addons/swiftlys2/plugins/Ankor/Ankor.dll \
 && printf '{ "core": true }\n' > /opt/ankor/overlay/addons/swiftlys2/configs/core.jsonc \
 && printf '{ "Chat": { "Tag": "Ankor", "TagColor": "Green" } }\n' > /opt/ankor/overlay/addons/swiftlys2/configs/plugins/Ankor/config.jsonc \
 && printf 'echo base\n' > /opt/ankor/overlay/cfg/ankor/base.cfg \
 && printf 'echo warmup\n' > /opt/ankor/overlay/cfg/ankor/warmup.cfg \
 && printf 'echo knife\n' > /opt/ankor/overlay/cfg/ankor/knife.cfg \
 && chmod 755 /opt/ankor/overlay/cfg/ankor \
 && touch -d '2026-01-01T00:00:00Z' $(find /opt/ankor/overlay) \
 && chown -R steam:steam /home/steam
COPY --from=build /out /opt/ankor/headless/
# The test engine reads the image's cfgs, a launch cfg and the chat catalog out of the plugin's
# source tree, at the path it was built from.
COPY cfg/ /src/cfg/
COPY resources/translations/ /src/resources/translations/
COPY tests/fixtures/ankor_launch.cfg /src/tests/fixtures/ankor_launch.cfg
COPY headless/docker/entrypoint.sh /usr/local/bin/entrypoint.sh
ENV HOME=/home/steam
USER steam
WORKDIR /home/steam
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
