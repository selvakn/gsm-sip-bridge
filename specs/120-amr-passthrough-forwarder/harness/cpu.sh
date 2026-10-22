#!/bin/sh
# usage: cpu.sh <window-seconds> -- <command prefix to reach the bridge container shell, e.g. docker exec NAME sh -c>
#   CPU seconds (user+system) the bridge's agent processes used over the window, per process and total.
#   Run during a steady call. Reads /proc/<pid>/stat inside the container.
win=$1; shift; shift
snap='for p in /proc/[0-9]*; do pid=${p#/proc/}; c=$(tr "\0" " " < $p/cmdline 2>/dev/null | cut -c1-160); case "$c" in *vowifi-ims-agent*|*vowifi-sip-agent*|*sip-agent*|*ims-agent*) set -- $(sed "s/.*) //" $p/stat); echo "$pid $((${12}+${13})) $(echo $c | sed "s|.*gsm-sip-bridge ||; s|--config [^ ]* ||")";; esac; done'
"$@" "$snap" > /tmp/cpu-a.$$ 2>/dev/null
sleep "$win"
"$@" "$snap" > /tmp/cpu-b.$$ 2>/dev/null
hz=$("$@" 'getconf CLK_TCK' 2>/dev/null || echo 100)
awk -v w="$win" -v hz="$hz" 'NR==FNR{a[$1]=$2;next}{d=($2-a[$1])/hz; t+=d; printf "pid %s %.2f cpu-s (%.1f%% of one core)  %s %s %s\n",$1,d,d*100/w,$3,$4,$5} END{printf "total %.2f cpu-s over %ss = %.1f%% of one core\n",t,w,t*100/w}' /tmp/cpu-a.$$ /tmp/cpu-b.$$
rm -f /tmp/cpu-a.$$ /tmp/cpu-b.$$
