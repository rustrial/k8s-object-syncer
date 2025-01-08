ARG ALPINE_VERSION=3.21.1

FROM alpine:$ALPINE_VERSION as builder

RUN apk --no-cache add ca-certificates libgcc gcc pkgconfig openssl-dev build-base curl

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y 

ENV RUST_CHANNEL=stable

ENV PATH=$PATH:/root/.cargo/bin

RUN rustup install $RUST_CHANNEL

WORKDIR /workdir

COPY . /workdir/

ENV RUSTFLAGS="-C target-feature=-crt-static"

RUN cargo +$RUST_CHANNEL build --release

FROM alpine:$ALPINE_VERSION as alpine

# Needed for RUSTFLAGS="-C target-feature=-crt-static" as above
RUN apk --no-cache add libgcc

# Cross compile arm64/aarch64 binaries created by docker buildx are linked to /lib/ld-linux-aarch64.so.1 
# which is called /lib/ld-musl-aarch64.so.1 on alpine.
RUN ( [[ $(uname -m) == "aarch64" ]] && ln -s /lib/ld-musl-aarch64.so.1 /lib/ld-linux-aarch64.so.1 ) || true

COPY --from=builder /workdir/target/release/rustrial-k8s-object-syncer /usr/local/bin/rustrial-k8s-object-syncer

ENTRYPOINT [ "/usr/local/bin/rustrial-k8s-object-syncer" ]

USER 1000

FROM scratch

COPY --from=builder /workdir/target/release/rustrial-k8s-object-syncer /usr/local/bin/rustrial-k8s-object-syncer

COPY --from=alpine /lib /lib

COPY --from=alpine /usr/lib /usr/lib

ENTRYPOINT [ "/usr/local/bin/rustrial-k8s-object-syncer" ]

USER 1000