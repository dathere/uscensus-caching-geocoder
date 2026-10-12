# syntax=docker/dockerfile:1
# Build: docker build --build-arg UCG_GIT_SHA=$(git rev-parse --short=12 HEAD) -t ucgeocoder .
FROM rust:1.99-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
ARG UCG_GIT_SHA=unknown
ENV UCG_GIT_SHA=${UCG_GIT_SHA}
RUN cargo build --release --locked \
    && cp target/release/ucgeocoder /ucgeocoder \
    && mkdir /data

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /ucgeocoder /usr/local/bin/ucgeocoder
COPY --from=build --chown=65532:65532 /data /data
ENV UCG_BIND=0.0.0.0:8080 \
    UCG_DB_DIR=/data
VOLUME ["/data"]
EXPOSE 8080
USER nonroot
ENTRYPOINT ["/usr/local/bin/ucgeocoder"]
CMD ["serve"]
