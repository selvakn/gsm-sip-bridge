FROM alpine:3.21
RUN apk add --no-cache build-base wget pkgconf openssl-dev alsa-lib-dev util-linux-dev linux-headers \
      opencore-amr-dev vo-amrwbenc-dev opus-dev tcpdump python3 bash
WORKDIR /src
RUN wget -q https://github.com/pjsip/pjproject/archive/refs/tags/2.16.tar.gz && tar xzf 2.16.tar.gz
COPY config-stock.h config-passthru.h /tmp/
ARG VARIANTS="stock passthru"
RUN for v in $VARIANTS; do \
      cp -a pjproject-2.16 build-$v && cd build-$v && \
      cp /tmp/config-stock.h pjlib/include/pj/config-stock.h && \
      cp /tmp/config-$v.h pjlib/include/pj/config_site.h && \
      ./configure --prefix=/opt/$v --disable-shared --enable-static --disable-video --disable-v4l2 \
        --with-external-pa=no --with-opencore-amr=/usr --with-opencore-amrwbenc=/usr --with-opus=/usr CFLAGS="-fPIC -g" LDFLAGS="$([ $v = passthru ] && echo -Wl,--allow-multiple-definition)" >/dev/null && \
      make -j"$(nproc)" dep >/dev/null && make -j"$(nproc)" >/dev/null && make install >/dev/null && \
      cp pjsip-apps/bin/pjsua-* /opt/$v/pjsua && cd /src; \
    done
