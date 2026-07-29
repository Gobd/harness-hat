# harness-hat Kotlin / JVM image
#
# Build (context: docker/ directory):
#   docker build -t harness-hat-kotlin:local -f docker/kotlin.dockerfile docker/
#
# Inside a session:
#   kotlinc Main.kt -include-runtime -d main.jar && java -jar main.jar
#   # Or via Gradle:
#   gradle build && gradle run

# The official Temurin and Gradle manifests are pinned and multi-architecture.
# Docker selects the correct target image without maintaining release filenames
# and hashes for every supported CPU architecture.
ARG TEMURIN_IMAGE=eclipse-temurin:21.0.7_6-jdk-noble@sha256:c04e695e59a97337e87d55ebbe9f527aacec1504b78ffac2730475057a8ea465
ARG GRADLE_IMAGE=gradle:8.14.2-jdk21@sha256:cf0caeaac06e2824fac26daa6a55728fcb0e0e0e2ff28a430d2292547fe84dff
FROM ${TEMURIN_IMAGE} AS temurin
FROM ${GRADLE_IMAGE} AS gradle

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

# ── JDK and Gradle ───────────────────────────────────────────────────────────
# Bump each image tag and manifest-list digest together when upgrading.
COPY --from=temurin /opt/java/openjdk /usr/local/lib/jvm/temurin-21
COPY --from=gradle /opt/gradle /opt/gradle
ENV JAVA_HOME=/usr/local/lib/jvm/temurin-21
ENV PATH="${JAVA_HOME}/bin:/opt/gradle/bin:${PATH}"
RUN java -version && gradle --version

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

ENV PATH="${JAVA_HOME}/bin:/opt/gradle/bin:/home/coder/.local/bin:${PATH}"
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
