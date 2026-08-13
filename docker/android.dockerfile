# harness-hat Kotlin / Android image
#
# Build after harness-hat-base:local:
#   docker build -t harness-hat-android:local -f docker/android.dockerfile docker/
#
# This image is intended for Android Gradle builds and command-line tooling.
# It does not include a graphical emulator; connect to a host emulator through
# a configured localhost forward when a project needs one.

# The official Temurin and Gradle manifests are pinned and multi-architecture.
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

# ── Kotlin compiler ──────────────────────────────────────────────────────────
ARG KOTLIN_VERSION=2.4.10
ARG KOTLIN_SHA256=473dd66c7a3ef4b182065b3da670466c1bf2773a9dbb0ed8b33a39fe9d4f876d

RUN set -eu; \
    curl -fsSL --retry 5 --retry-all-errors --retry-delay 2 \
        -o /tmp/kotlin-compiler.zip \
        "https://github.com/JetBrains/kotlin/releases/download/v${KOTLIN_VERSION}/kotlin-compiler-${KOTLIN_VERSION}.zip"; \
    echo "${KOTLIN_SHA256}  /tmp/kotlin-compiler.zip" | sha256sum -c -; \
    unzip -q /tmp/kotlin-compiler.zip -d /usr/local/lib; \
    ln -sf /usr/local/lib/kotlinc/bin/kotlinc /usr/local/bin/kotlinc; \
    ln -sf /usr/local/lib/kotlinc/bin/kotlin /usr/local/bin/kotlin; \
    rm -f /tmp/kotlin-compiler.zip; \
    kotlinc -version

# ── Android SDK command-line tools ───────────────────────────────────────────
# Google publishes the Linux command-line tools as an x86_64 archive. Docker
# Desktop can run this image under its amd64 compatibility mode on ARM hosts.
# The SDK packages are pre-warmed for a current stable Android build target;
# projects can install additional versions with sdkmanager at runtime.
ARG ANDROID_CMDLINE_TOOLS_VERSION=15859902
ARG ANDROID_CMDLINE_TOOLS_SHA256=4e4c464f145a7512b57d088ac6c278c03c9eea610886b35a5e0804e74eedf583
ENV ANDROID_HOME=/opt/android-sdk
ENV ANDROID_SDK_ROOT=/opt/android-sdk
ENV PATH="${ANDROID_HOME}/cmdline-tools/latest/bin:${ANDROID_HOME}/platform-tools:${JAVA_HOME}/bin:/opt/gradle/bin:${PATH}"

RUN set -eu; \
    mkdir -p "${ANDROID_HOME}/cmdline-tools" /tmp/android-cmdline; \
    curl -fsSL --retry 5 --retry-all-errors --retry-delay 2 \
        -o /tmp/android-command-line-tools.zip \
        "https://dl.google.com/android/repository/commandlinetools-linux-${ANDROID_CMDLINE_TOOLS_VERSION}_latest.zip"; \
    echo "${ANDROID_CMDLINE_TOOLS_SHA256}  /tmp/android-command-line-tools.zip" | sha256sum -c -; \
    unzip -q /tmp/android-command-line-tools.zip -d /tmp/android-cmdline; \
    mv /tmp/android-cmdline/cmdline-tools "${ANDROID_HOME}/cmdline-tools/latest"; \
    yes | sdkmanager --sdk_root="${ANDROID_HOME}" --licenses >/dev/null; \
    sdkmanager --sdk_root="${ANDROID_HOME}" \
        "platform-tools" \
        "platforms;android-36" \
        "build-tools;36.0.0"; \
    rm -rf /tmp/android-command-line-tools.zip /tmp/android-cmdline; \
    sdkmanager --sdk_root="${ANDROID_HOME}" --list >/dev/null; \
    adb version; \
    sdkmanager --version

# Keep SDK and Gradle caches writable by the session user. The Android SDK is
# shared read/write within the container but never bind-mounted from the host.
RUN mkdir -p /home/coder/.gradle /home/coder/.android \
    && chown -R coder:coder "${ANDROID_HOME}" /home/coder/.gradle /home/coder/.android

ENV GRADLE_USER_HOME=/home/coder/.gradle
ENV PATH="${ANDROID_HOME}/cmdline-tools/latest/bin:${ANDROID_HOME}/platform-tools:${JAVA_HOME}/bin:/opt/gradle/bin:/home/coder/.local/bin:${PATH}"

USER coder

# Initialise Gradle wrapper infrastructure for the coder user so the first
# system-gradle invocation in a session does not stall downloading its daemon.
RUN gradle --version --no-daemon

CMD ["/bin/bash"]
