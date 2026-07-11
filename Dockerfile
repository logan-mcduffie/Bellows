FROM rust:1.92-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --locked --release -p bellows-server

FROM debian:bookworm-slim
RUN useradd --system --create-home --uid 10001 bellows
COPY --from=build /src/target/release/bellowsd /usr/local/bin/bellowsd
USER bellows
VOLUME ["/var/lib/bellows"]
EXPOSE 7878
ENTRYPOINT ["bellowsd", "--listen", "0.0.0.0:7878", "--data-dir", "/var/lib/bellows"]

