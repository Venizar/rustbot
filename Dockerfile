FROM rust:latest AS builder

WORKDIR /usr/src/my_rust_bot
COPY . .

# в release-режиме
RUN cargo build --release

# Этап 2: Создание финального образа на базе Debian
FROM debian:stable-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends libssl3 ca-certificates && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Копирование скомпилированный бинарник из этапа сборки
COPY --from=builder /usr/src/my_rust_bot/target/release/my_rust_bot .

# Копируем файлы html
COPY static ./static
# Копируем файл .env
COPY .env .

# Папки локал и дата
COPY locales ./locales
COPY data ./data
# ----------------------------------------------------

# Открываем порт для веб-интерфейса, потом обратным гипервизором пробросить и будет чудо
EXPOSE 6001

# Команда для запуска приложения
CMD ["./my_rust_bot"]
