#!/bin/bash
# One-shot benchmark run on a fresh, disposable Ubuntu 24.04 machine (run
# prepare.sh first). Installs packages, changes kernel and CPU frequency
# settings, and writes results/. Do not run this on a machine you care about.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p results
log() { echo "[$(date +%T)] $*" | tee -a results/progress.log; }

log "setup"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq build-essential "linux-tools-$(uname -r)" linux-tools-common >/dev/null
if ! command -v cargo >/dev/null; then
    curl -sSf https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain stable
fi
. "$HOME/.cargo/env"
sysctl -q kernel.perf_event_paranoid=-1
sysctl -q kernel.nmi_watchdog=0 || true
for g in /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor; do echo performance > "$g" 2>/dev/null || true; done
{ nproc; lscpu | grep -E "Model name|Socket|Core|Thread|L3"; rustc --version; } > results/machine.txt

log "build"
cargo build --release -q

log "bench"
./target/release/slabbench > results/bench.txt
log "bench done"

log "perf stat"
events=cycles,instructions,cache-references,cache-misses,L1-dcache-load-misses,branch-misses
# Physical cores: contended benchmarks run one thread per core.
cores=$(lscpu -p=CORE | grep -v '^#' | sort -u | wc -l)
for v in $(sed -n 's/^\([a-z0-9_]*\) = { package = "sharded-slab".*/\1/p' Cargo.toml); do
    for spec in "get 1" "get_hot $cores" "get_interleaved $cores" "pool_remote_clear $cores"; do
        set -- $spec
        perf stat -x, -e "$events" -o "results/perf-$v-$1-$2.csv" \
            ./target/release/slabbench "$v" "$1" "$2" >> results/perf-bench.txt
    done
done
log "all done"
