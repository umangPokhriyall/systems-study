#!/usr/bin/env bash
# Emit the environment manifest that must accompany every committed results file.
#
# A number without one of these is not evidence (docs/METHODOLOGY.md, rule 1). The point is not
# that a laptop measurement is as good as a bare-metal one - it is not - but that a reader can see
# exactly which it was and discount it correctly.
#
# Usage: tools/capture-env.sh > rung-NN-x/results/env-$(hostname)-$(date +%F).txt

set -uo pipefail

section() { printf '\n== %s ==\n' "$1"; }

printf 'captured: %s\n' "$(date -Is)"
printf 'host:     %s\n' "$(hostname)"

section kernel
uname -srvmo
printf 'cmdline: %s\n' "$(cat /proc/cmdline 2>/dev/null)"
[ -r /etc/os-release ] && grep -E '^(PRETTY_NAME)=' /etc/os-release

section cpu
lscpu | grep -E 'Model name|^CPU\(s\)|Thread|Core|Socket|NUMA node\(s\)|MHz|Virtualization|Flags' \
      | sed 's/  */ /g' | cut -c1-160

section topology
# Core-to-L3 map. On a chiplet part this is what determines whether two threads share a cache,
# which is the difference between a cheap and an expensive cross-core handoff.
lscpu -e=CPU,CORE,SOCKET,NODE,L3 2>/dev/null \
  || lscpu -e 2>/dev/null \
  || echo "(lscpu -e unavailable)"

section numa
numactl --hardware 2>/dev/null || echo "(numactl not installed; assume a single node)"

section frequency-and-thermal
for p in /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor \
         /sys/devices/system/cpu/intel_pstate/no_turbo \
         /sys/devices/system/cpu/cpufreq/boost; do
  [ -r "$p" ] && printf '%s = %s\n' "$p" "$(cat "$p")"
done
printf 'note: an unpinned governor and active turbo mean the tail of any distribution below\n'
printf '      includes frequency transitions. That is a property of the machine, not an error.\n'

section perf-access
printf 'kernel.perf_event_paranoid = %s\n' "$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null)"
printf 'kernel.kptr_restrict       = %s\n' "$(cat /proc/sys/kernel/kptr_restrict 2>/dev/null)"

section memory
free -h | head -2
printf 'transparent_hugepage = %s\n' "$(cat /sys/kernel/mm/transparent_hugepage/enabled 2>/dev/null)"

section kvm
if [ -e /dev/kvm ]; then
  ls -l /dev/kvm
  printf 'readable-writable by this user: '
  if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then echo yes; else echo no; fi
  [ -d /sys/module/kvm_intel ] && echo "kvm_intel loaded"
  [ -d /sys/module/kvm_amd ] && echo "kvm_amd loaded"
  # Nested and unrestricted-guest change what a real-mode guest costs, so they belong in the record.
  for m in /sys/module/kvm_intel/parameters/{nested,unrestricted_guest} \
           /sys/module/kvm_amd/parameters/nested; do
    [ -r "$m" ] && printf '%s = %s\n' "$m" "$(cat "$m")"
  done
else
  echo "/dev/kvm absent"
fi

section load
uptime
printf 'note: a non-idle machine inflates the tail. Record it rather than retrying until quiet.\n'

section toolchain
rustc --version 2>/dev/null
cargo --version 2>/dev/null

section repository
git -C "$(dirname "$0")/.." rev-parse HEAD 2>/dev/null || echo "(not a git repository yet)"
if ! git -C "$(dirname "$0")/.." diff --quiet 2>/dev/null; then
  echo "WORKING TREE DIRTY - results from this session are not attributable to a commit"
fi
