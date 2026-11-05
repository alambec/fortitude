# Fortitude server image.
#
#   docker build -t fortitude .
#   docker run -p 8042:8042 -v fortitude-data:/var/lib/fortitude fortitude

FROM rust:1.92-bookworm AS build
# CMake: CharLS, the JPEG-LS codec, is built from source.
RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p fortitude \
    && cp target/release/fortitude /usr/local/bin/fortitude

FROM debian:bookworm-slim
# CA certificates: HTTPS to S3.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN useradd --system --uid 10001 --home-dir /var/lib/fortitude fortitude \
    && mkdir -p /var/lib/fortitude \
    && chown fortitude /var/lib/fortitude
COPY --from=build /usr/local/bin/fortitude /usr/local/bin/fortitude
USER fortitude
ENV FORTITUDE_STORAGE__PATH=/var/lib/fortitude \
    FORTITUDE_LOG__FORMAT=json
EXPOSE 8042
VOLUME /var/lib/fortitude
ENTRYPOINT ["fortitude"]
