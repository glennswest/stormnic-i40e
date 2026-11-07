# stormnic-i40e

A Rust `no_std` UEFI Simple Network Protocol (SNP) driver for **Intel i40e (X710, XL710, XXV710, X722)** (8086:1572, 1583/1584, 1589, 158B, 15FF, 37D0–37D3), loaded by
[stormbootx](https://github.com/glennswest/stormbootx) so it can claim and boot images over these NICs without iPXE.
A sibling of stormnic-ixgbe, stormnic-mlx4 and stormnic-virtio, built and pinned the same way (stormbootx `build-nic-drivers.sh`).

Status: not started (P3). #1 waits on the owner: no lab machine or VM has an i40e to verify on (2026-11-07).

## Licence

MIT (see LICENSE). NOTICE acknowledges the sources the hardware facts were learned from; no code was copied.
