#!/usr/bin/env bash
# A/B test of the HTTP/2 initial stream window against a real h2 origin.
#
# The claim under test: hyper's default 64 KiB stream window caps a single
# HTTP/2 stream at window/RTT, regardless of how much bandwidth is available.
# On a high-latency path that ceiling is far below link speed, so a downloader
# that leaves the default in place is slow for a reason nothing reports.
#
# The two runs differ in exactly one setting. Configurations alternate rather
# than running in blocks, so a drift in network conditions partway through
# cannot be mistaken for an effect.
#
#   ./h2-window-ab.sh [url] [runs]

set -u

URL="${1:-https://proof.ovh.net/files/10Mb.dat}"
RUNS="${2:-3}"
IDM="$(cd "$(dirname "$0")/../.." && pwd)/target/release/idm.exe"
SMALL=65536          # hyper's default
LARGE=$((8 * 1024 * 1024))

if [ ! -x "$IDM" ]; then
  echo "build the CLI first: cargo build --release -p idm-cli" >&2
  exit 1
fi

# One connection only: the window is a per-stream limit, so any concurrency
# would mask exactly the effect being measured.
run_one() {
  local window="$1" out rate
  out="$(mktemp -d)"
  rate=$("$IDM" "$URL" -o "$out" --name ab.bin -n 1 --h2-window "$window" --json 2>/dev/null \
    | grep -o '"bytes_per_sec":[0-9.]*' | cut -d: -f2)
  rm -rf "${out:?}"
  echo "${rate:-0}"
}

median() {
  printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}'
}

echo "HTTP/2 stream window A/B"
echo "  url   $URL"
echo "  runs  $RUNS per configuration, alternating"
echo

rtt=$(curl -sS -o /dev/null -w "%{time_connect}" --max-time 20 "$URL" 2>/dev/null)
echo "  tcp connect (~1 RTT): ${rtt}s"
python -c "
rtt = float('${rtt}')
for name, win in (('64 KiB default', $SMALL), ('8 MiB tuned', $LARGE)):
    print(f'  predicted ceiling, {name:<16}: {win/rtt/1e6:8.2f} MB/s')
" 2>/dev/null || true
echo

small_rates=(); large_rates=()
for i in $(seq 1 "$RUNS"); do
  s=$(run_one "$SMALL");  small_rates+=("$s")
  printf "  run %s  64 KiB: %8.0f B/s" "$i" "$s"
  l=$(run_one "$LARGE");  large_rates+=("$l")
  printf "   |  8 MiB: %8.0f B/s\n" "$l"
done

sm=$(median "${small_rates[@]}")
lg=$(median "${large_rates[@]}")
echo
python -c "
sm, lg = float('$sm'), float('$lg')
print(f'  median 64 KiB default : {sm/1e6:6.2f} MB/s')
print(f'  median 8 MiB tuned    : {lg/1e6:6.2f} MB/s')
if sm > 0:
    print(f'  speedup from the fix  : {lg/sm:6.2f}x')
"
