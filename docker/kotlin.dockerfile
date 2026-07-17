# harness-hat Kotlin / JVM image
#
# Build (context: docker/ directory):
#   docker build -t harness-hat-kotlin:local -f docker/kotlin.dockerfile docker/
#
# Inside a session:
#   kotlinc Main.kt -include-runtime -d main.jar && java -jar main.jar
#   # Or via Gradle:
#   gradle build && gradle run

FROM harness-hat-base:local
USER root

RUN apt-get update -o APT::Update::Error-Mode=any \
    && apt-get install -y --no-install-recommends \
       ca-certificates \
       curl \
       unzip \
       build-essential \
       make \
       jq \
       shellcheck \
       direnv \
    && rm -rf /var/lib/apt/lists/*

# ── JDK ──────────────────────────────────────────────────────────────────────
# Pinned Adoptium Temurin 21 LTS (H5). Bump by updating ARGs and sha256s.
ARG JDK_VERSION=21.0.7+6
ARG JDK_VERSION_FILENAME=21.0.7_6
ARG JDK_SHA256_X64=9b5b4346c1fa5fd46d23bdbb8e3abd00f7dfdf5d5c50bdd61bd0e4f791a9f39c
ARG JDK_SHA256_AARCH64=cafc27a5289e24a20dab0a6cec4b90ed6040a7ef04daed6e7c3c1d6cd9ee0a5e
ARG TARGETARCH

RUN set -eu; \
    case "${TARGETARCH:-$(dpkg --print-architecture)}" in \
        amd64|x86_64)  jdk_arch="x64";     jdk_sha="${JDK_SHA256_X64}" ;; \
        arm64|aarch64) jdk_arch="aarch64"; jdk_sha="${JDK_SHA256_AARCH64}" ;; \
        *) echo "unsupported JDK architecture: ${TARGETARCH:-$(dpkg --print-architecture)}" >&2; exit 1 ;; \
    esac; \
    curl -fsSL \
        -o /tmp/jdk.tar.gz \
        "https://github.com/adoptium/temurin21-binaries/releases/download/jdk-${JDK_VERSION}/OpenJDK21U-jdk_${jdk_arch}_linux_hotspot_${JDK_VERSION_FILENAME}.tar.gz"; \
    echo "${jdk_sha}  /tmp/jdk.tar.gz" | sha256sum -c -; \
    mkdir -p /usr/local/lib/jvm; \
    tar -xzf /tmp/jdk.tar.gz -C /usr/local/lib/jvm; \
    ln -sf "/usr/local/lib/jvm/jdk-${JDK_VERSION}/bin/java" /usr/local/bin/java; \
    ln -sf "/usr/local/lib/jvm/jdk-${JDK_VERSION}/bin/javac" /usr/local/bin/javac; \
    ln -sf "/usr/local/lib/jvm/jdk-${JDK_VERSION}/bin/jar" /usr/local/bin/jar; \
    rm -f /tmp/jdk.tar.gz; \
    java -version

ENV JAVA_HOME=/usr/local/lib/jvm/jdk-${JDK_VERSION}

# ── Kotlin compiler ───────────────────────────────────────────────────────────
# Pinned kotlinc (H5). Bump by editing ARG and sha256.
ARG KOTLIN_VERSION=2.4.10
ARG KOTLIN_SHA256=473dd66c7a3ef4b182065b3da670466c1bf2773a9dbb0ed8b33a39fe9d4f876d

RUN set -eu; \
    curl -fsSL \
        -o /tmp/kotlin-compiler.zip \
        "https://github.com/JetBrains/kotlin/releases/download/v${KOTLIN_VERSION}/kotlin-compiler-${KOTLIN_VERSION}.zip"; \
    echo "${KOTLIN_SHA256}  /tmp/kotlin-compiler.zip" | sha256sum -c -; \
    unzip -q /tmp/kotlin-compiler.zip -d /usr/local/lib; \
    ln -sf /usr/local/lib/kotlinc/bin/kotlinc /usr/local/bin/kotlinc; \
    ln -sf /usr/local/lib/kotlinc/bin/kotlin  /usr/local/bin/kotlin; \
    rm -f /tmp/kotlin-compiler.zip; \
    kotlinc -version

# ── Gradle ────────────────────────────────────────────────────────────────────
# Pinned Gradle (H5). Bump by editing ARG and sha256.
ARG GRADLE_VERSION=8.14.2
ARG GRADLE_SHA256=2b585f4b5da9c778a78d8cc04a0eefd38d1e5e7e748a52f7b38ee3a0d7d17b5f

RUN set -eu; \
    curl -fsSL \
        -o /tmp/gradle.zip \
        "https://services.gradle.org/distributions/gradle-${GRADLE_VERSION}-bin.zip"; \
    echo "${GRADLE_SHA256}  /tmp/gradle.zip" | sha256sum -c -; \
    unzip -q /tmp/gradle.zip -d /usr/local/lib; \
    ln -sf "/usr/local/lib/gradle-${GRADLE_VERSION}/bin/gradle" /usr/local/bin/gradle; \
    rm -f /tmp/gradle.zip; \
    gradle --version

ENV PATH="/usr/local/bin:/home/coder/.local/bin:${PATH}"
# Gradle stores its cache under ~/.gradle. Pre-warm the Foojay toolchain
# resolver plugin so projects that use `java { toolchain { languageVersion } }`
# can auto-download the right JDK without extra setup. This runs as root so
# the Gradle home ends up at /root/.gradle; the USER switch below means agent
# sessions use /home/coder/.gradle instead — warm-up happens on first build.
# The resolver itself is fetched from plugins.gradle.org at first use.
ENV GRADLE_USER_HOME=/home/coder/.gradle

USER coder

# Pre-initialise Gradle wrapper infrastructure for the coder user so the first
# `gradle` invocation in a session doesn't stall downloading the daemon jar.
# `gradle --version` performs this initialisation without a project context.
RUN gradle --version --no-daemon

CMD ["/bin/bash"]
