# syntax=docker/dockerfile:1.11
ARG BUILDER=base

FROM docker.io/library/rust:1.98.0-trixie AS base-builder

ARG TARGETARCH

RUN <<EOF
mkdir /build
if [ "$TARGETARCH" = "arm64" ]; then
  echo aarch64-unknown-linux-gnu > /build/target
else
  echo x86_64-unknown-linux-gnu > /build/target
fi
echo "Building $(cat /build/target)"
EOF

FROM ${BUILDER}-builder AS builder
ARG TARGETARCH
ARG PROFILE=release
ARG VERSION
ARG GIT_REVISION

WORKDIR /app

COPY . ./

RUN \
    --mount=type=cache,id=cargo,target=/usr/local/cargo/registry \
    --mount=type=cache,id=cargo-git,target=/usr/local/cargo/git \
    cargo fetch --locked
RUN --mount=type=cache,target=/app/target \
    --mount=type=cache,id=cargo,target=/usr/local/cargo/registry  \
    --mount=type=cache,id=cargo-git,target=/usr/local/cargo/git \
    <<EOF
export VERSION="${VERSION}"
export GIT_REVISION="${GIT_REVISION}"

cargo build --target "$(cat /build/target)" --profile ${PROFILE} || exit 1

mkdir /out
mv /app/target/$(cat /build/target)/${PROFILE}/git-serve /out
# /out/git-serve --version || exit 1
# # Fail if version is not set
# if /out/git-serve --version | grep -q '"version": "unknown"'; then
#   exit 1
# fi
EOF

FROM cgr.dev/chainguard/git AS runner

ARG TARGETARCH

WORKDIR /

COPY --from=builder /out/git-serve /app/git-serve

ENTRYPOINT ["/app/git-serve"]