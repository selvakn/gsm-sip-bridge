#!/bin/sh
set -e

chown bridge:bridge /data
exec gsm-sip-bridge "$@"
