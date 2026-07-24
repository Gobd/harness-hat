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

# ── Install a .NET SDK ────────────────────────────────────────────────────────
# Installs into $DOTNET_ROOT. Multiple invocations of this block side-by-side
# in the same DOTNET_ROOT — each SDK version occupies its own subdirectory.
ARG TARGETARCH

# .NET 10 (LTS — active until 2028-11-14).
# Bump by updating ARGs and checksums from:
#   https://dotnetcli.blob.core.windows.net/dotnet/release-metadata/10.0/releases.json
ARG DOTNET10_VERSION=10.0.302
ARG DOTNET10_SHA512_X64=10069bec8783596484a610332f090d562802a41b9b40e3327a5a5688b572e10c296ae300f940d40461f23c157ed1b0843c2f8e6b3f20d8d8d9d83432d8143bac
ARG DOTNET10_SHA512_AARCH64=9e409c14e00686d661c78fa4dd9ad0e4dcf695c328bd5ff777d05b4a9c34b42cf89b12573b92e9fb2f565dbe12016b4835f77c7d9a42b55a7494df21634cd5d6

RUN set -eu; \
    case "${TARGETARCH:-$(dpkg --print-architecture)}" in \
        amd64|x86_64)  dotnet_arch="x64";   dotnet_sha="${DOTNET10_SHA512_X64}" ;; \
        arm64|aarch64) dotnet_arch="arm64"; dotnet_sha="${DOTNET10_SHA512_AARCH64}" ;; \
        *) echo "unsupported architecture: ${TARGETARCH:-$(dpkg --print-architecture)}" >&2; exit 1 ;; \
    esac; \
    case "${#dotnet_sha}" in \
        64) dotnet_sha_cmd="sha256sum" ;; \
        128) dotnet_sha_cmd="sha512sum" ;; \
        *) echo "unsupported checksum length for dotnet SHA: ${#dotnet_sha}" >&2; exit 1 ;; \
    esac; \
    curl -fsSL \
        -o /tmp/dotnet.tar.gz \
        "https://builds.dotnet.microsoft.com/dotnet/Sdk/${DOTNET10_VERSION}/dotnet-sdk-${DOTNET10_VERSION}-linux-${dotnet_arch}.tar.gz"; \
    printf '%s  /tmp/dotnet.tar.gz\n' "${dotnet_sha}" | "${dotnet_sha_cmd}" -c -; \
    mkdir -p "${DOTNET_ROOT}"; \
    tar -xzf /tmp/dotnet.tar.gz -C "${DOTNET_ROOT}"; \
    rm -f /tmp/dotnet.tar.gz; \
    dotnet --version

# .NET 8 (maintenance — supported until 2026-11-10).
# Provides the SDK + runtime for projects with global.json pinned to 8.x.
# Bump by updating ARGs and checksums from:
#   https://dotnetcli.blob.core.windows.net/dotnet/release-metadata/8.0/releases.json
ARG DOTNET8_VERSION=8.0.423
ARG DOTNET8_SHA512_X64=e94513dfe42271a85f01e87bd4272aa80b4ec13556f4531754802542225667775242c5e281a94837dae6cc65f7bcc457d2f663f240c0e2b7573fd909e786b1a5
ARG DOTNET8_SHA512_AARCH64=8c6dd335a8fa63849af551fe6f10ca8e92db0b1aaa761727e3d997d7ebfebe68d9fdccdd241a1804f6812055770b00a40648f32e69f1606ecf864440902c67a1

RUN set -eu; \
    case "${TARGETARCH:-$(dpkg --print-architecture)}" in \
        amd64|x86_64)  dotnet_arch="x64";   dotnet_sha="${DOTNET8_SHA512_X64}" ;; \
        arm64|aarch64) dotnet_arch="arm64"; dotnet_sha="${DOTNET8_SHA512_AARCH64}" ;; \
        *) echo "unsupported architecture: ${TARGETARCH:-$(dpkg --print-architecture)}" >&2; exit 1 ;; \
    esac; \
    case "${#dotnet_sha}" in \
        64) dotnet_sha_cmd="sha256sum" ;; \
        128) dotnet_sha_cmd="sha512sum" ;; \
        *) echo "unsupported checksum length for dotnet SHA: ${#dotnet_sha}" >&2; exit 1 ;; \
    esac; \
    curl -fsSL \
        -o /tmp/dotnet.tar.gz \
        "https://builds.dotnet.microsoft.com/dotnet/Sdk/${DOTNET8_VERSION}/dotnet-sdk-${DOTNET8_VERSION}-linux-${dotnet_arch}.tar.gz"; \
    printf '%s  /tmp/dotnet.tar.gz\n' "${dotnet_sha}" | "${dotnet_sha_cmd}" -c -; \
    tar -xzf /tmp/dotnet.tar.gz -C "${DOTNET_ROOT}"; \
    rm -f /tmp/dotnet.tar.gz; \
    dotnet --list-sdks

ENV PATH="${DOTNET_ROOT}:/home/coder/.local/bin:${PATH}"

USER coder
CMD ["/bin/bash"]
