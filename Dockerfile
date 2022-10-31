FROM rust:1-slim-bullseye AS build
ARG DEBIAN_FRONTEND=noninteractive

ADD . /app
WORKDIR /app
RUN apt-get update \
  && apt-get install -y pkg-config libssl-dev \
  && cargo build --release

FROM debian:bullseye-slim
ARG DEBIAN_FRONTEND=noninteractive

RUN apt update && apt install -y libssl1.1 ca-certificates
COPY --from=build /app/target/release/lava-gitlab-runner /usr/local/bin

ENTRYPOINT [ "/usr/local/bin/lava-gitlab-runner" ]
