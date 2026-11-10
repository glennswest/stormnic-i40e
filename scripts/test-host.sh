#!/usr/bin/env bash
# The UEFI-independent core (PF reset, admin queue, HMC, rings, SNP state
# machine) against the simulated device in test/sim.rs, on the build box:
#
#   sc-build scripts/test-host.sh
set -euo pipefail
cd "$(dirname "$0")/.."
out="${CARGO_TARGET_DIR:-target}/host-tests"
mkdir -p "$out"
for t in i40e snp trace; do
    rustc --edition=2021 -D warnings --test "test/$t.rs" -o "$out/$t-tests"
    "$out/$t-tests"
done
