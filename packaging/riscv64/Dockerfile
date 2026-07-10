# Cross-compile rt for riscv64 (riscv64gc-unknown-linux-gnu).
#
# There are no native riscv64 CI runners, and rt links the Wayland/GL/font stack,
# so we need a *sysroot* carrying those libraries for the target. Debian 13
# (trixie) has riscv64 as an official release architecture, so `dpkg
# --add-architecture riscv64` + apt gives us the :riscv64 -dev libraries from the
# main mirror — a real multiarch sysroot in one rootfs, no separate sysroot dir.
FROM debian:trixie-slim

ARG RUST_TARGET=riscv64gc-unknown-linux-gnu

# Host build tools + the riscv64 GCC cross toolchain + the target's -dev libs.
RUN dpkg --add-architecture riscv64 \
 && apt-get update \
 && apt-get install -y --no-install-recommends \
      curl ca-certificates pkg-config \
      gcc libc6-dev \
      gcc-riscv64-linux-gnu libc6-dev-riscv64-cross \
      libwayland-dev:riscv64 \
      libxkbcommon-dev:riscv64 \
      libgl-dev:riscv64 libegl-dev:riscv64 \
      libfontconfig-dev:riscv64 \
      libx11-dev:riscv64 libxcb1-dev:riscv64 \
 && rm -rf /var/lib/apt/lists/*

# Rust via rustup (pinned to stable) + the riscv64 std target.
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
ENV PATH=/root/.cargo/bin:$PATH
RUN rustup target add ${RUST_TARGET}

# Point cargo/cc/pkg-config at the cross toolchain and the riscv64 multiarch libs.
# The libs live under /usr/lib/riscv64-linux-gnu in the same rootfs, so pkg-config
# just needs that path plus permission to cross (ALLOW_CROSS), no sysroot dir.
ENV CARGO_TARGET_RISCV64GC_UNKNOWN_LINUX_GNU_LINKER=riscv64-linux-gnu-gcc \
    CC_riscv64gc_unknown_linux_gnu=riscv64-linux-gnu-gcc \
    AR_riscv64gc_unknown_linux_gnu=riscv64-linux-gnu-ar \
    PKG_CONFIG_ALLOW_CROSS=1 \
    PKG_CONFIG_PATH=/usr/lib/riscv64-linux-gnu/pkgconfig:/usr/share/pkgconfig

WORKDIR /work
