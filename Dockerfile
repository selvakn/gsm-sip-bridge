FROM debian:bookworm-slim AS pjsip

ARG PJSIP_VERSION=2.16

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

FROM pjsip AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    cmake \
    git \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY CMakeLists.txt Makefile ./
COPY src/ src/
COPY tests/ tests/

RUN cmake -B build -DCMAKE_BUILD_TYPE=Release -S . \
    && cmake --build build --parallel

RUN cmake -B build-test -DCMAKE_BUILD_TYPE=Debug -S . \
    && cmake --build build-test --parallel \
    && cd build-test && ctest --output-on-failure

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    libasound2 \
    libssl3 \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/local/lib/*.so* /usr/local/lib/
RUN ldconfig

COPY --from=builder /src/build/gsm-sip-bridge /usr/local/bin/gsm-sip-bridge
COPY --from=builder /src/build/gsm-echo /usr/local/bin/gsm-echo
COPY --from=builder /src/build/sip-echo /usr/local/bin/sip-echo
COPY config.ini.example /etc/gsm-sip-bridge/config.ini.example

EXPOSE 9091

ENTRYPOINT ["gsm-sip-bridge"]
CMD ["--config", "/etc/gsm-sip-bridge/config.ini"]
