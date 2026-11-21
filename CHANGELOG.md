# Changelog

## [Unreleased]

### 2026-11-20
- **fix:** #9 (finding 9b7243b167eb): `Fec::of_config` took the FEC ability bits (which encodings the PHY supports) as requests, so a down 25G port whose configuration had only RS ability (0x02) counted as already configured for RS and was never written, leaving an RS partner down; and a port of other media with abilities only counted as off its standard. An encoding is now read as asked for only by its request bit with its ability (or auto); the simulator's running FEC follows the same rule
- **test:** test/i40e.rs: configuration bytes with abilities only, requests only and both; a down 25G port with RS ability only gets RS requested and links; a down 10G port with abilities only is not written
- **fix:** #8 (finding beafcf3a3ebd): GetStatus handed a transmit token back (and set TRANSMIT_INTERRUPT) as soon as the frame was queued, whether or not the device had sent it. Each token now waits with its descriptor: it is recycled, and TRANSMIT reported, only once the device has written that descriptor back done (`Net::tx_retired`)
- **fix:** #8 (finding e7839b0884f0): Shutdown, Stop, Reset, re-Initialize, halt and teardown left the tokens of discarded frames queued, so the next Initialize's GetStatus returned them as sent and repeated cycles filled the recycle queue. Every ring stop or restart now forgets them
- **test:** test/snp.rs: a stalled NIC recycles nothing until it sends; GetStatus without recycle keeps done buffers; Shutdown/Reset/re-Initialize/Stop discard unsent tokens; 192 shutdown cycles never fill the recycle queue
- **fix:** #7 (finding 704188c2551d, critical): after a teardown that failed (binding Stop, or a Start failure after bring-up or SNP install), `keep_dma` turned bus mastering off and the PCI restore right after put the original attributes and command bits back, so a function that had BME on before Start got it back on over the abandoned DMA region. `decode::release` now takes whether the DMA region was kept: then memory decode and the other attributes are restored but BME never is, and BME is checked and cleared again at the end
- **test:** test/decode.rs (in `scripts/test-host.sh`): enable/release against a fake PCI function, quiet release restores BME as found, kept-DMA release leaves BME off by attributes, by command register, without `Get`, and when firmware's `Set` turns it back on

### 2026-11-17
- **docs:** refreshed from the code (`git log --since=2026-11-10`: only #5 and its docs landed): README's example port line carries the built version 0.1.0 (was 0.2.0); README, docs/hardware.md and CLAUDE.md otherwise already match src/port.rs, the 4000 ms link wait and the console; nothing promised that the code does not do (hardware verification stays #2)

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
