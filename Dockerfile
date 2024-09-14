FROM rust:latest as build

COPY judge-core/ /usr/src/judge-core
COPY judger/     /usr/src/judger

WORKDIR /usr/src/judger
RUN apt update && apt install -y libseccomp-dev gcc
RUN cargo build --bin judger --release


FROM ubuntu:latest

RUN apt update && apt install -y libseccomp-dev gcc g++ curl unzip
COPY --from=build /usr/src/judger/target/release/judger /usr/local/bin/judger

RUN curl https://rclone.org/install.sh | bash

RUN mkdir /workdir
RUN mkdir /workdir/problem-package
COPY judger/.env /workdir/.env
COPY judger/rclone.conf /workdir/rclone.conf
RUN sed -i 's/127.0.0.1/host.docker.internal/g' /workdir/rclone.conf

WORKDIR /workdir
ENV RUST_LOG=DEBUG
ENV PLATFORM_URI=http://host.docker.internal:8080/
ENV ENABLE_RCLONE=true
EXPOSE 8000
CMD [ "judger", "serve" ]