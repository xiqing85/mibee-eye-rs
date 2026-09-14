#!/usr/bin/env bash
# Indicative footprint benchmark: samples RSS + CPU% of the running camera
# service while an RTSP client pulls the stream.
#
# Usage (on the device, with ffmpeg installed):
#   ./bench/rpi-bench.sh [binary-name] [rtsp-url] [seconds]
# Defaults: mibee-eye-raspi-rs / rtsp://127.0.0.1:8554/stream / 60
#
# Method: one ffmpeg client pulls the RTSP stream over TCP for the whole
# run; every 5 s we sample /proc/<pid>/status (VmRSS) and the utime+stime
# jiffies from /proc/<pid>/stat, converting the delta into CPU %.
# Reported: max/avg RSS, avg CPU %, plus the sample count.
set -euo pipefail

BIN="${1:-mibee-eye-raspi-rs}"
URL="${2:-rtsp://127.0.0.1:8554/stream}"
SECS="${3:-60}"
HZ=$(getconf CLK_TCK)

# Kernel comm fields are capped at 15 chars, so pgrep -x can never match a
# longer binary name — fall back to the truncated comm before failing.
PID="$(pgrep -x "${BIN}" | head -n1 || true)"
[ -n "${PID}" ] || PID="$(pgrep -x "${BIN:0:15}" | head -n1 || true)"
[ -n "${PID}" ] || { echo "error: ${BIN} is not running" >&2; exit 1; }

ffmpeg -hide_banner -loglevel error -rtsp_transport tcp -i "${URL}" \
       -t "${SECS}" -f null - & CLIENT=$!
trap 'kill "${CLIENT}" 2>/dev/null || true' EXIT
sleep 2

jiffies() { awk -v p="${PID}" '$1==p{print $14+$15}' "/proc/${PID}/stat"; }
rss_kb()  { awk '/^VmRSS:/{print $2}' "/proc/${PID}/status"; }

t0=$(date +%s); c0=$(jiffies)
max_rss=0; sum_rss=0; n=0
echo "sampling ${BIN} (pid ${PID}) for ~${SECS}s while ${URL} is pulled ..."
while kill -0 "${CLIENT}" 2>/dev/null; do
  sleep 5
  rss=$(rss_kb) || continue
  t1=$(date +%s); c1=$(jiffies)
  cpu=$(awk -v dc=$((c1 - c0)) -v dt=$((t1 - t0)) -v hz="${HZ}" \
       'BEGIN{ if (dt>0) printf "%.1f", (dc/hz)/dt*100; else print "0" }')
  echo "  rss=${rss}kB cpu=${cpu}%"
  [ "${rss}" -gt "${max_rss}" ] && max_rss="${rss}"
  sum_rss=$((sum_rss + rss)); n=$((n + 1))
  t0=${t1}; c0=${c1}
done

[ "${n}" -gt 0 ] || { echo "error: no samples collected" >&2; exit 1; }
awk -v m="${max_rss}" -v s="${sum_rss}" -v n="${n}" 'BEGIN{
  printf "\nRESULT  maxRSS=%.1fMB  avgRSS=%.1fMB  samples=%d\n", m/1024, s/n/1024, n
}'
