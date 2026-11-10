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

## Version

`Cargo.toml` → `package.version`. Planned first: `v0.1.0`.

## Work plan

- [ ] #1 — UEFI SNP driver: bind, link, transmit/receive (simulation-verified)
  - [ ] Scaffold: Cargo, build.rs (subsystem 11), glue from stormnic-igb,
        PCI IDs (FreeBSD i40e_devids.h + pci.ids: 37d4, 0dda)
  - [ ] Core: PF reset, admin queue (version, clear PXE, MAC read, switch
        config, VSI params, promiscuous modes, link status/restart AN),
        HMC (paged SD, one PD page, one backing page) and queue contexts,
        rings (32-byte RX, 16-byte TX, WB_ON_ITR write-back), SNP core
  - [ ] Simulated X710 + firmware model, host tests (`scripts/test-host.sh`)
  - [ ] `sc-build scripts/check-driver.sh` (x86_64 + aarch64, clippy, PE32+ 11)
  - [ ] docs/hardware.md, README, CHANGELOG, v0.1.0
  - [ ] Hardware verification as its own issue; stormbootx pin only after it
        (not on the rustnic media until then)
