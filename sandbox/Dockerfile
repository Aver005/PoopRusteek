# Sandbox image for the headless harness.
#
# Why a container at all: a harness run answers tool approvals from a policy,
# and the tool set includes a shell. On a developer machine that is a loaded
# gun; here the blast radius is a throwaway filesystem.
#
# Two stages. The builder needs the full Rust toolchain and downloads ONNX
# Runtime as part of building `fastembed`; the runtime image carries only the
# binary, that shared library, and the handful of CLI tools the agent's shell
# calls are expected to find.

# syntax=docker/dockerfile:1

# --- builder ---------------------------------------------------------------
# Pinned to the project's MSRV (edition 2024 needs it). **Trixie, not
# bookworm**: the prebuilt static ONNX Runtime that `ort` downloads is linked
# against glibc >= 2.38, which redirects `strtoll`/`strtoull` to the C23
# symbols `__isoc23_strtoll`/`__isoc23_strtoull`. Bookworm ships glibc 2.36
# and has neither, so linking fails with a wall of `undefined symbol` errors
# from `libort_sys`. Both stages must stay on the same (or newer) glibc.
FROM rust:1.91-trixie AS builder

WORKDIR /src

# Manifests first so the dependency layer survives source edits.
COPY Cargo.toml Cargo.lock* ./
COPY assets ./assets

# BuildKit cache mounts keep the registry and the target dir across builds —
# without them every rebuild recompiles wasmtime, ort and syntect from
# scratch, which is many minutes each time. The target dir is copied out at
# the end because a cache mount is not part of the image.
COPY src ./src

# `dev` rebuilds in a fraction of the time (release is lto="fat" +
# codegen-units=1), which matters when the point of the sandbox is fast
# iteration. Release stays the default for realistic timings.
ARG BUILD_PROFILE=release
# Capped on purpose. Unbounded parallel rustc plus the ort/wasmtime link step
# exhausts the WSL2 VM's memory and takes the Docker daemon down with it —
# the same failure the repo's CLAUDE.md documents for host builds
# (STATUS_STACK_BUFFER_OVERRUN / "paging file too small"). Raise it if the
# machine has headroom to spare.
ARG CARGO_BUILD_JOBS=4
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    set -eux; \
    cargo build --profile "${BUILD_PROFILE}" --jobs "${CARGO_BUILD_JOBS}" --bin pooprusteek; \
    if [ "${BUILD_PROFILE}" = "dev" ]; then OUT=target/debug; else OUT="target/${BUILD_PROFILE}"; fi; \
    mkdir -p /out; \
    cp "${OUT}/pooprusteek" /out/; \
    find target -name 'libonnxruntime*.so*' -exec cp {} /out/ \; || true

# --- runtime ---------------------------------------------------------------
FROM debian:trixie-slim AS runtime

# `bash` and `git` because the agent's shell tool reaches for them; `curl`
# and `ca-certificates` for the provider and the model download; `jq` because
# every debugging session inside the sandbox ends up reading a JSONL trace.
#
# `python3` is here for the scenarios, not the app: the interesting failures of
# a coding agent happen in the red-to-green loop (run the tests, read the
# error, edit, run again), and without a real toolchain in the image that loop
# cannot be exercised at all. The stdlib covers it with no install step —
# `unittest` and `http.server` are enough for a realistic task.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      bash ca-certificates curl git jq procps python3 tini \
 && rm -rf /var/lib/apt/lists/*

# A fixed non-root uid so bind-mounted volumes have predictable ownership.
ARG UID=1000
ARG GID=1000
RUN groupadd -g "${GID}" agent \
 && useradd -m -u "${UID}" -g "${GID}" -s /bin/bash agent

COPY --from=builder /out/pooprusteek /usr/local/bin/pooprusteek
COPY --from=builder /out/ /tmp/libs/
RUN find /tmp/libs -name 'libonnxruntime*.so*' -exec cp {} /usr/local/lib/ \; \
 && rm -rf /tmp/libs \
 && ldconfig

# XDG paths decide where `dirs::config_dir()` and `dirs::data_dir()` land, so
# they are set explicitly rather than left to depend on the base image.
ENV HOME=/home/agent \
    XDG_CONFIG_HOME=/home/agent/.config \
    XDG_DATA_HOME=/home/agent/.local/share \
    POOPRUSTEEK_SANDBOX=1 \
    RUST_BACKTRACE=1

COPY --chown=agent:agent sandbox/config.template.toml /opt/sandbox/config.template.toml
COPY --chown=agent:agent sandbox/entrypoint.sh /opt/sandbox/entrypoint.sh
COPY --chown=agent:agent sandbox/scenarios /opt/sandbox/scenarios
COPY --chown=agent:agent sandbox/mock-scripts /opt/sandbox/mock-scripts
COPY --chown=agent:agent sandbox/fixtures /opt/sandbox/fixtures
# Skills come from the repo so RAG scenarios have something real to retrieve.
COPY --chown=agent:agent skills /opt/sandbox/skills
RUN chmod +x /opt/sandbox/entrypoint.sh \
 && mkdir -p /home/agent/.config/pooprusteek \
             /home/agent/.local/share/pooprusteek \
             /work /out \
 && chown -R agent:agent /home/agent /work /out

USER agent
WORKDIR /work

# tini reaps the shell children the agent's tools spawn; without it a
# backgrounded job leaves a zombie for the lifetime of the container.
ENTRYPOINT ["/usr/bin/tini", "--", "/opt/sandbox/entrypoint.sh"]
CMD ["bash"]
