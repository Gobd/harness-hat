# harness-hat PHP image
#
# Build after harness-hat-base:local:
#   docker build -t harness-hat-php:local -f docker/php.dockerfile .

FROM harness-hat-base:local

USER root

ENV COMPOSER_HOME=/home/coder/.composer
ENV PATH="${COMPOSER_HOME}/vendor/bin:${PATH}"

RUN set -eu; \
    apt-get update -o APT::Update::Error-Mode=any; \
    apt-get install -y --no-install-recommends \
      build-essential \
      make \
      pkg-config \
      jq \
      shellcheck \
      direnv \
      sqlite3 \
      libsqlite3-dev \
      php-cli \
      php-dev \
      php-curl \
      php-mbstring \
      php-xml \
      php-zip \
      php-intl \
      php-bcmath \
      php-gd \
      php-mysql \
      php-pgsql \
      php-sqlite3 \
      php-soap \
      php-xdebug \
      php-pcov; \
    rm -rf /var/lib/apt/lists/*

RUN set -eu; \
    curl -fsSL https://getcomposer.org/installer -o /tmp/composer-setup.php; \
    php /tmp/composer-setup.php --install-dir=/usr/local/bin --filename=composer; \
    rm -f /tmp/composer-setup.php; \
    mkdir -p "${COMPOSER_HOME}"; \
    chown -R coder:coder "${COMPOSER_HOME}"

USER coder

ENV COMPOSER_HOME=/home/coder/.composer
ENV PATH="${COMPOSER_HOME}/vendor/bin:/home/coder/.local/bin:${PATH}"

RUN set -eu; \
    composer global require \
      phpunit/phpunit \
      friendsofphp/php-cs-fixer \
      phpstan/phpstan \
      laravel/pint

CMD ["bash"]
