# harness-hat C# / .NET image
#
# Build (context: docker/ directory):
#   docker build -t harness-hat-csharp:local -f docker/csharp.dockerfile docker/
#
# Inside a session:
#   dotnet new console -n MyApp && cd MyApp
#   dotnet add package Newtonsoft.Json
#   dotnet run
#
# SDK version selection via global.json:
#   .NET supports multiple installed SDKs. If a project has a global.json that
#   pins an older SDK version, dotnet will use it automatically. Add a
#   global.json to the workspace root to pin:
#     { "sdk": { "version": "8.0.400", "rollForward": "latestFeature" } }
#   Both .NET 10 (LTS) and .NET 8 (maintenance) SDKs are installed here so
#   either version works out of the box.

# Source the SDKs from Microsoft's official multi-architecture images instead
# of maintaining separate download URLs and checksums for every architecture.
# The manifest-list digests make the source immutable while still allowing
# Docker to select amd64 or arm64 automatically. When upgrading an SDK, update
# its version tag and manifest-list digest together.
ARG DOTNET10_IMAGE=mcr.microsoft.com/dotnet/sdk:10.0.302-noble@sha256:ed034a8bf0b24ded0cbbac07e17825d8e9ebfe21e308191d0f7421eaf5ad4664
ARG DOTNET8_IMAGE=mcr.microsoft.com/dotnet/sdk:8.0.423-noble@sha256:283164eecee1fc80a590410d3c56207af455bd51b8bf3cbf2ded3592b9014b29
FROM ${DOTNET10_IMAGE} AS dotnet10
FROM ${DOTNET8_IMAGE} AS dotnet8

FROM harness-hat-base:local
USER root

RUN apt-get update -o APT::Update::Error-Mode=any \
    && apt-get install -y --no-install-recommends \
       ca-certificates \
       curl \
       build-essential \
       make \
       libicu-dev \
       libssl-dev \
       jq \
       shellcheck \
       direnv \
    && rm -rf /var/lib/apt/lists/*

ENV DOTNET_ROOT=/usr/local/lib/dotnet
ENV PATH="${DOTNET_ROOT}:${PATH}"
# Suppress telemetry and first-run welcome message inside containers.
ENV DOTNET_CLI_TELEMETRY_OPTOUT=1
ENV DOTNET_NOLOGO=1
# NuGet cache inside the workspace volume so packages persist across sessions.
ENV NUGET_PACKAGES=/workspace/.nuget/packages

# ── Install .NET SDKs ────────────────────────────────────────────────────────
# .NET 10 (LTS — active until 2028-11-14) and .NET 8 (maintenance — supported
# until 2026-11-10) share DOTNET_ROOT so global.json selects either SDK.
COPY --from=dotnet10 /usr/share/dotnet/ ${DOTNET_ROOT}/
COPY --from=dotnet8 /usr/share/dotnet/ ${DOTNET_ROOT}/
RUN dotnet --list-sdks

ENV PATH="${DOTNET_ROOT}:/home/coder/.local/bin:${PATH}"

USER coder
CMD ["/bin/bash"]
