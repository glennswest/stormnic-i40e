# stormnic-i40e

A Rust `no_std` UEFI Simple Network Protocol (SNP) driver for **Intel i40e (X710, XL710, XXV710, X722)** (27 Intel PF device IDs, listed in `src/ids.rs`; the virtual functions and the pre-silicon 1574 are not bound), loaded by
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
4. brings the port up itself (the stormnic contract, #5): reads the media
   (Get Link Status PHY type) and applies the IEEE 802.3 standard FEC for
   it, RS-FEC (Clause 108) on 25G media and none on 1G/2.5G/5G/10G/20G/40G,
   with Set PHY Config when the port does not already run it; then waits
   for the link (up to 4 s), restarting autonegotiation if it is down.
   Nothing about FEC, speed or the port is configured from outside;
5. runs a DMA check (one broadcast frame) when the link is up;
6. installs `EFI_SIMPLE_NETWORK_PROTOCOL` on a child handle.

SNP Initialize starts one queue pair (32 x 2 KiB buffers each way, polled,
write-back on ITR); receive filters map to the VSI's promiscuous modes plus
an exact software filter; link changes come as admin-queue events.
ExitBootServices and Stop stop the queues, turn the admin queue off and
reset the PF before any memory is given back.

Console: one line per port, the stormnic contract's: driver and version,
PCI address, PCI ID, part, port, MAC, media, speed, FEC (what runs) and
link, e.g.
`stormnic-i40e 0.2.0: 0000:01:00.1 8086:158b XXV710 port 1: MAC 02:11:22:33:44:55, media 25GBASE-SR, speed 25 Gb/s, FEC rs, link UP`.
A port without link, or off its standard FEC, is an error line:
`…, media 25GBASE-SR, speed none, FEC rs, link DOWN: error: no link after 4000 ms (the link partner must run RS-FEC too, IEEE 802.3 Clause 108; check the cable and the switch port)`;
no module (`media none … no module or cable`), a module the firmware does
not run, and a refused Set PHY Config are said the same way. The firmware
version and "verified in simulation only" are in the trace. The full trace with
`StormnicVerbose` set or `--features verbose`. The variable lives under the vendor GUID shared by all stormnic drivers (`ce1479a2-eab9-4176-b0ad-c909ea5b8e0b`), so one `setvar` turns them all verbose. Without it, trace lines are kept and printed only ahead of a failure.

See docs/hardware.md for the hardware facts used.

## Build and test

```bash
sc-build scripts/check-driver.sh   # x86_64 + aarch64, clippy -D warnings, PE32+ subsystem 11
sc-build scripts/test-host.sh      # core against the simulated device
```

## Licence

MIT (see LICENSE). NOTICE acknowledges the sources the hardware facts were learned from; no code was copied.
