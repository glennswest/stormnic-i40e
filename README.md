# stormnic-i40e

A Rust `no_std` UEFI Simple Network Protocol (SNP) driver for **Intel i40e (X710, XL710, XXV710, X722)** (8086:1572, 1583/1584, 1589, 158B, 15FF, 37D0–37D3), loaded by
[stormbootx](https://github.com/glennswest/stormbootx) so it can claim and boot images over these NICs without iPXE.
A sibling of stormnic-ixgbe, stormnic-mlx4 and stormnic-virtio, built and pinned the same way (stormbootx `build-nic-drivers.sh`).

Status: **v0.1.0, verified in simulation only.** No 700-series NIC is in the
lab, and QEMU emulates none, so the driver is tested against a simulated
X710 (`test/sim.rs`: registers, a firmware model answering the admin queue,
the HMC, the queues). It is **not on the rustnic media** until it has run on
a device (decision on #1); that check is its own issue.

## What it does

An EFI boot-service driver installing `EFI_DRIVER_BINDING_PROTOCOL`. For
each 700-series PF that no other driver holds, Start:

1. clears queues and interrupts a previous driver left, and resets the PF;
2. sets up the firmware admin queue and asks for the firmware version
   (API 1.x), PXE mode off, the LAN MAC address, the VSI and its first queue;
3. adds a MAC filter for any VLAN, lays out the HMC (queue contexts in one
   host page);
4. reads the link, restarting autonegotiation (up to 4 s) if it is down;
5. runs a DMA check (one broadcast frame) when the link is up;
6. installs `EFI_SIMPLE_NETWORK_PROTOCOL` on a child handle.

SNP Initialize starts one queue pair (32 x 2 KiB buffers each way, polled,
write-back on ITR); receive filters map to the VSI's promiscuous modes plus
an exact software filter; link changes come as admin-queue events.
ExitBootServices and Stop stop the queues, turn the admin queue off and
reset the PF before any memory is given back.

Console: one line per NIC (`... firmware 9.1 API 1.15, MAC ..., link up 10
Gb/s, SNP installed (verified in simulation only)`); the full trace with
`StormnicVerbose` set or `--features verbose`.

See docs/hardware.md for the hardware facts used.

## Build and test

```bash
sc-build scripts/check-driver.sh   # x86_64 + aarch64, clippy -D warnings, PE32+ subsystem 11
sc-build scripts/test-host.sh      # core against the simulated device
```

## Licence

MIT (see LICENSE). NOTICE acknowledges the sources the hardware facts were learned from; no code was copied.
