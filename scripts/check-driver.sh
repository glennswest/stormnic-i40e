#!/usr/bin/env bash
# Build the driver for x86_64 and aarch64 and check each image is what the
# firmware needs: a PE32+ EFI boot-service driver (subsystem 11) for the
# right machine, not an application (10), which the firmware would unload as
# soon as its entry point returned. Clippy runs on both targets with
# warnings as errors.
#
#   sc-build scripts/check-driver.sh
#
# SC_BUILD_OUT can collect dist/: the two .efi files and their sha256.
set -euo pipefail
cd "$(dirname "$0")/.."

# The aarch64 std, from rustup when the toolchain lacks it (as stormbootx's
# build-golden.sh does until the build template carries it, stormcentral#604).
for t in x86_64-unknown-uefi aarch64-unknown-uefi; do
    if [[ ! -d "$(rustc --print sysroot)/lib/rustlib/$t" ]]; then
        command -v rustup >/dev/null || { echo "no $t std and no rustup to add it" >&2; exit 1; }
        echo "adding the $t std with rustup"
        rustup target add "$t" >/dev/null
    fi
done

target_dir="${CARGO_TARGET_DIR:-target}"
mkdir -p dist
for t in x86_64-unknown-uefi aarch64-unknown-uefi; do
    # --locked: Cargo.lock is committed; a build must not re-resolve deps.
    cargo build --locked --release --target "$t"
    cargo clippy --locked --release --target "$t" -- -D warnings
    cp "$target_dir/$t/release/stormnic-i40e.efi" "dist/stormnic-i40e-${t%%-*}.efi"
done

python3 - dist/stormnic-i40e-x86_64.efi 0x8664 dist/stormnic-i40e-aarch64.efi 0xaa64 <<'PY'
import hashlib, struct, sys
args = sys.argv[1:]
names = {10: "EFI_APPLICATION", 11: "EFI_BOOT_SERVICE_DRIVER", 12: "EFI_RUNTIME_DRIVER"}
for path, want in zip(args[::2], args[1::2]):
    want = int(want, 16)
    data = open(path, "rb").read()
    pe = struct.unpack_from("<I", data, 0x3c)[0]
    assert data[pe:pe + 4] == b"PE\0\0", "not a PE image"
    machine = struct.unpack_from("<H", data, pe + 4)[0]
    opt = pe + 24
    magic = struct.unpack_from("<H", data, opt)[0]
    subsystem = struct.unpack_from("<H", data, opt + 68)[0]
    print(f"{path}: {len(data)} bytes, sha256 {hashlib.sha256(data).hexdigest()}")
    print(f"  machine 0x{machine:x}, magic 0x{magic:x} (PE32+ is 0x20b), subsystem {subsystem} ({names.get(subsystem, '?')})")
    assert machine == want and magic == 0x20b, f"not a PE32+ image for machine 0x{want:x}"
    assert subsystem == 11, "not linked as an EFI boot-service driver"
    print("  OK: EFI boot-service driver")
PY
( cd dist && sha256sum stormnic-i40e-*.efi > SHA256SUMS )
