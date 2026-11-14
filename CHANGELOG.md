# Changelog

## [Unreleased]

### 2026-11-13
- **feat:** #5 the stormnic contract: the driver brings its port up itself with the IEEE standard FEC for its media/speed (25G → RS-FEC Clause 108, 1G–10G/20G/40G → none), via Get PHY Abilities (0x0600) and Set PHY Config (0x0601) only when the port does not already run it; one line per port (driver, PCI address, PCI ID, part, port, MAC, media, speed, FEC running, link), with no link, no module, an unsupported module, a refused or unread FEC, or a port off its standard FEC said as an `error:` line; firmware version moves to the trace
- **test:** the simulated device has a media, a PHY configuration with FEC, Set PHY Config and a 25G link partner that links only on its FEC; 12 new tests (standard FEC table, configuration bytes, kept/written/refused/unread/no PHY types, no module, unsupported module, the port line, events carrying the FEC)
- **fix:** the simulator's PHY type 0x1e was labelled 10GBASE-SR (it is 20GBASE-KR2); it now reports 0x14

### 2026-11-10
- **docs:** refreshed from the code: PF ID count is 27 (was 26), README names the ID source and the shared `StormnicVerbose` GUID
- **docs:** #2 work plan: no 700-series NIC in the lab yet (asset list re-checked); hardware check waits on a card

## [v0.1.0] — 2026-11-10

Verified in simulation only (no 700-series NIC in the lab; hardware check is #2). Not on the rustnic media.

### Added
- #1 UEFI SNP driver for the 700 series (X710/XL710/XXV710/X722, 27 PF IDs from FreeBSD i40e_devids.h + pci.ids): PF reset, firmware admin queue (version, clear PXE, MAC, switch config, VSI, MAC filter, promiscuous modes, link status/events, restart AN, queue shutdown), HMC with one paged SD, one queue pair with write-back on ITR, SNP state machine, binding glue from stormnic-igb
- **fix:** clippy `is_multiple_of` (hmc, ring); SNP tests start with an empty wire
- simulated X710 with a firmware model (`test/sim.rs`); device and SNP host tests (`scripts/test-host.sh`)

### Documentation
- docs/hardware.md, README, NOTICE (sources: FreeBSD ixl shared code; iPXE behaviour only)
- #1 decided (master for the owner): simulation-tested, off the rustnic media until it runs on a device

### 2026-11-07
- **docs:** CLAUDE.md with the work plan for #1; no i40e device or emulating VM in the lab, so how #1 is verified is asked of the owner on #1

### 2026-10-07
- **chore:** repository created (owner: low priority, paused)
