# CLAUDE.md — stormnic-i40e

A `no_std` UEFI driver giving firmware an `EFI_SIMPLE_NETWORK_PROTOCOL` for
Intel X710 / XL710 / XXV710 / X722 (the Linux `i40e` family), loaded by
stormbootx from `\stormboot\drivers` on its rustnic media. A sibling of
stormnic-ixgbe (the template: binding, decode, rings, SNP, quiet console,
reproducible build) and stormnic-mlx4/mlx5/virtio. Read README.md first.

Read the cross-project rules in `../CLAUDE.md` first: build with `sc-build`
after pushing, never on this VM and never as root; scratch files in `tmp/`.

## Rules for this crate (planned, as stormnic-ixgbe)

- **Sources:** Intel's public X710/XL710 and X722 datasheets for registers,
  the admin queue and descriptor formats; Intel's own BSD-3-Clause shared code
  as published in FreeBSD `sys/dev/ixl` for admin-queue command sequences
  (the precedent: stormnic-ixgbe#2, and the owner's choice of FreeBSD
  `sys/dev/ice` for stormbootx#72); the SNP from the UEFI specification.
  **Never translated from iPXE `intelxl.c` or Linux `i40e`** (GPL): the crate
  is MIT. Reading them for behaviour is fine.
- An EFI boot-service driver (subsystem 11) installing
  `EFI_DRIVER_BINDING_PROTOCOL`; a platform's own driver wins (BY_DRIVER open
  of PciIo, decline if held).
- Console: one line per NIC by default, the trace behind `StormnicVerbose` /
  `--features verbose` (as stormnic-ixgbe#22).

## Hardware

**No i40e device in the lab (checked 2026-11-07).** `stormcentral asset list`
shows only 82599 (8086:1557, server1/7/8), tg3 (14e4:165f, C2NR0Q2, forge1)
and Mellanox (C2NR0Q2 15b3:1015, forge1) NICs; QEMU emulates no i40e part.
#1 asks for a VM or device check before closing, so how #1 is verified is the
owner's decision (question on #1).

## Version

`Cargo.toml` → `package.version` (no crate yet). Planned first: `v0.1.0`.

## Work plan

- [ ] #1 — UEFI SNP driver: bind, link, transmit/receive; pinned in
  stormbootx's `build-nic-drivers.sh`
  - **Waiting on the owner, 2026-11-07.** No lab machine or VM can run an
    i40e (see Hardware). Asked on #1: (1) write it from the datasheets + BSD
    `ixl` code, simulation-tested, not on the rustnic media until it runs on a
    device, close #1 on simulation with hardware verification its own issue
    (as stormnic-ixgbe#23 and stormbootx#72); (2) the owner fits an X710/XL710
    card in a test machine first; (3) stay paused. Recommended: (1). No code
    until answered.
  - Plan once answered (stormnic-ixgbe's structure): `ids` (the issue's PFs 1572,
    1583/1584, 1589, 158b, 15ff, 37d0–37d3, plus those stormbootx#71's
    pci.ids check found: 1580/1581, 1585–1588, 158a, 104e/104f, 101f,
    37ce/37cf, 37d4; re-check against pci.ids when coding starts), `binding`, `decode`, `adminq` (ATQ/ARQ rings, get version,
    clear PXE mode, get switch config, VSI parameters, set MAC config,
    link status, MAC address read), `rings` (queue context in HMC/LAN context
    via the admin queue, 32-byte RX / 16-byte TX descriptors, QTX_ENA/QRX_ENA),
    `snp_core`/`snp`, `console`/`trace`; a simulated device with an admin-queue
    firmware model under `test/`; `scripts/check-driver.sh`,
    `scripts/test-hardware.sh`, `scripts/check-reproducible.sh`.
