FROM debian:bookworm-slim

ARG PJSIP_VERSION=2.17

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    pkg-config \
    libasound2-dev \
    libssl-dev \
    wget \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN wget -q https://github.com/pjsip/pjproject/archive/refs/tags/${PJSIP_VERSION}.tar.gz \
    && tar xzf ${PJSIP_VERSION}.tar.gz \
    && cd pjproject-${PJSIP_VERSION} \
    && ./configure --prefix=/usr/local \
        --enable-shared \
        --disable-video \
        --disable-v4l2 \
        --with-external-pa=no \
    && make -j$(nproc) dep \
    && make -j$(nproc) \
    && make install \
    && ldconfig \
    && cd / && rm -rf pjproject-${PJSIP_VERSION} ${PJSIP_VERSION}.tar.gz
