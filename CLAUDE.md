# CLAUDE.md — stormnic-i40e

A `no_std` UEFI driver giving firmware an `EFI_SIMPLE_NETWORK_PROTOCOL` for
Intel X710 / XL710 / XXV710 / X722 (the Linux `i40e` family), loaded by
stormbootx from `\stormboot\drivers` once it has run on a device (not on the
rustnic media before that). A sibling of stormnic-igb (the template: binding,
decode, rings, SNP, quiet console) and stormnic-ixgbe/mlx4/mlx5/virtio. Read
README.md and docs/hardware.md first.

Read the cross-project rules in `../CLAUDE.md` first: build with `sc-build`
after pushing, never on this VM and never as root; scratch files in `tmp/`.

## Hardware

**No i40e device in the lab (checked 2026-11-07).** `stormcentral asset list`
shows only 82599 (8086:1557, server1/7/8), tg3 (14e4:165f, C2NR0Q2, forge1)
and Mellanox (C2NR0Q2 15b3:1015, forge1) NICs; QEMU emulates no i40e part.
**Decision (master for the owner, 2026-11-10, on #1): option 1.** Written from
the datasheets + BSD `ixl` shared code, tested against a simulated device,
closed on simulation; it stays **off the rustnic media** until it has run on a
device, and hardware verification is its own issue (as stormnic-ixgbe#23,
stormbootx#72).

## Rules for this crate

- **Sources:** Intel's BSD-3-Clause shared code as published in FreeBSD
  `sys/dev/ixl` (commit 4b732459f030, 2026-11-05) for registers, admin-queue
  commands, the HMC and the queue contexts; pci.ids 2026.11.09 for the IDs; the
  UEFI specification for the binding and SNP. iPXE `intelxl.c` and Linux
  `i40e` (GPL) are read for behaviour only, **never translated**: the crate is
  MIT. docs/hardware.md says the hardware facts in its own words.
- Same layout as stormnic-igb: a UEFI-independent core (`src/i40e.rs`,
  `src/adminq.rs`, `src/hmc.rs`, `src/ring.rs`, `src/snp_core.rs`) tested on
  the host against a simulated X710 with a firmware model (`test/`); the UEFI
  glue (`binding.rs`, `snp.rs`, `pci_io.rs`) from stormnic-igb.
- EFI boot-service driver (subsystem 11); BY_DRIVER | EXCLUSIVE open of
  PciIo, so a platform driver that holds the NIC wins.
- The NIC is quiet whenever nobody drives it: queues disabled at every Start
  failure, SNP Shutdown/Stop, binding Stop and ExitBootServices; the admin
  queue is disabled and the PF reset before any DMA memory is freed or the OS
  takes over.
- Console: one line per NIC by default, the trace behind `StormnicVerbose` /
  `--features verbose`.

## Build and test

```bash
sc-build scripts/check-driver.sh   # x86_64 + aarch64 build, clippy -D warnings, PE32+ subsystem 11
sc-build scripts/test-host.sh      # core against the simulated X710 (test/sim.rs)
```

## Version

`Cargo.toml` → `package.version`. Current: `v0.1.0`.

## Work plan

- [x] #1 — UEFI SNP driver: bind, link, transmit/receive (closed 2026-11-10 on
      simulation, master's decision for the owner)
  - [x] Scaffold from stormnic-igb, 27 PF IDs (FreeBSD i40e_devids.h + pci.ids)
  - [x] Core: PF reset, admin queue, HMC, rings with WB_ON_ITR, SNP core
  - [x] Simulated X710 + firmware model; `sc-build scripts/test-host.sh` at
        7e902ea: 49 pass (i40e 30, snp 16, trace 3)
  - [x] `sc-build scripts/check-driver.sh` at 1529945: x86_64 65,536 B
        (sha256 fc51e1bd…f976), aarch64 61,440 B, PE32+ subsystem 11, clippy clean
  - [x] docs/hardware.md, README, NOTICE, CHANGELOG, v0.1.0
- [ ] #2 — run it on a real 700-series NIC (none in the lab); only then pin in
      stormbootx's nic-drivers (stormbootx#71) and put it on the rustnic media
      — waiting on a card (2026-11-10: asset list still shows only 82599, tg3
      and ConnectX-3/-4 Lx NICs); parked with `wait-owner --until`.
