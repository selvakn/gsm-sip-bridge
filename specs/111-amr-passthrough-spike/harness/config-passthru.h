#include "config-stock.h"
/* Spike #111: 1:1 switchboard + framing-only (passthrough) codecs. */
#define PJMEDIA_CONF_USE_SWITCH_BOARD 1
#define PJMEDIA_HAS_PASSTHROUGH_CODECS 1
/* Keep the software G.711/G.722 codecs (pjmedia-codec/config.h:370-391
 * disables them when their passthrough twins are on). */
#define PJMEDIA_HAS_PASSTHROUGH_CODEC_PCMU 0
#define PJMEDIA_HAS_PASSTHROUGH_CODEC_PCMA 0
#define PJMEDIA_HAS_PASSTHROUGH_CODEC_G722 0
