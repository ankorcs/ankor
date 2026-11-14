# The cs2 image the conformance suite launches in place of ankor-cs2: the plugin's headless image,
# which runs the plugin's core on the test engine (packages/plugin/headless), told where the suite
# drives it from. The agent starts game and update containers itself, so the suite cannot add env to
# them; it is baked in here instead.
# Build the headless image first, from packages/plugin, then this one from packages/agent:
#   docker build -f headless/docker/Dockerfile -t ankor-cs2-headless:latest .
#   docker build -f crates/conformance/docker/fake-cs2.Dockerfile -t ankor-conformance/fake-cs2:latest .

ARG HEADLESS_IMAGE=ankor-cs2-headless:latest
FROM ${HEADLESS_IMAGE}
ENV ANKOR_HEADLESS_CONTROL_URL=ws://controller.conformance.test/conformance/plugin
ENV ANKOR_HEADLESS_UPDATE_INSTRUCTION=/tmp/conformance-update.json
# The engine's clock moves only when a scenario says, from 2026-11-02T12:00:00Z, so demo names and
# game times are the same every run.
ENV ANKOR_HEADLESS_CLOCK=manual
ENV ANKOR_HEADLESS_START_MS=1793620800000
