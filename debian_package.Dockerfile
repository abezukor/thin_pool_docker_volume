# Intentionally use a old version of debian so the glibc is compatible with newer debian versions
FROM rust:slim-bullseye AS package_builder

ENV DEBIAN_FRONTEND=noninteractive

RUN apt update && apt install -y --no-install-recommends clang-19 curl pkg-config libblkid-dev

RUN curl -L --proto '=https' --tlsv1.2 -sSf https://raw.githubusercontent.com/cargo-bins/cargo-binstall/main/install-from-binstall-release.sh | bash
RUN cargo binstall cargo-deb

RUN mkdir /work
COPY --exclude=.git \
    --exclude=target \
    --exclude=debian_package.Dockerfile \
    --exclude=build_debian_package.sh \
    --exclude=debian_package \
    --exclude=.direnv \
    --exclude=flake.nix \
    --exclude=flake.lock \
    --exclude=nix \
    . /work
RUN cd /work && cargo deb

FROM scratch AS package_output
COPY --from=package_builder /work/target/debian/*.deb /
