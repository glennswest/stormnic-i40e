# The Intel 700 series, as this driver uses it

The hardware facts the driver relies on, in our own words. Learned from
Intel's BSD-3-Clause shared code (FreeBSD `sys/dev/ixl`, commit
4b732459f030); iPXE's `intelxl.c` was read for behaviour only. Every offset
below is in memory BAR 0. **Nothing here has been checked on silicon** (no
700-series NIC in the lab): the driver is verified against the simulator in
`test/sim.rs`, which models these same facts.

## 1. Which PF this is

| Register | Offset | Use |
|---|---|---|
| PF_FUNC_RID | 0x9C000 | bits 2:0: the PCI function, used as the PF index (QTX_CTL, GLHMC_*) |
| PFGEN_PORTNUM | 0x1C0480 | bits 1:0: physical port |
| PFLAN_QALLOC | 0x1C0400 | FIRSTQ 10:0, LASTQ 26:16, VALID 31: the PF's absolute LAN queues |

Per-queue registers (QTX_*, QRX_*, QINT_*) are indexed by the PF-relative
queue number; GLLAN_TXPRE_QDIS takes the absolute one (FIRSTQ + queue).

## 2. Reset

1. Clear what a previous driver left: PFINT_ICR0_ENA (0x38800) = 0,
   PFINT_DYN_CTL0 (0x38480) = ITR index 3 (none), PFINT_LNKLST0 (0x38500) =
   0x7FF (end of list); for each of the PF's queues, pre-disable transmit
   (below), wait 400 µs, then zero QINT_TQCTL, QTX_ENA, QINT_RQCTL, QRX_ENA.
2. If GLGEN_RSTAT (0xB8188) DEVSTATE (1:0) is non-zero, a global reset is
   running: poll every 100 ms, up to GLGEN_RSTCTL (0xB8180) GRSTDEL (5:0) x
   20 polls, at most 160. If one was running, no PF reset is needed.
3. Wait (10 ms polls, up to 10 s) for GLNVM_ULD (0xB6008) CORE_DONE (bit 3)
   and GLOBAL_DONE (bit 4): the firmware has loaded.
4. Set PFGEN_CTRL (0x92400) PFSWR (bit 0); it clears when the PF reset is
   done (1 ms polls, up to 1 s). A global reset starting meanwhile is waited
   out instead.

A PF reset clears the PF's queues, interrupts and HMC segment descriptors.
It does **not** clear the admin queue registers (EMP reset only), so the
driver turns those off itself.

## 3. Admin queue

Two rings of 32-byte descriptors in host memory:

| | Base low/high | Length | Head | Tail |
|---|---|---|---|---|
| ATQ (commands) | 0x80000 / 0x80100 | 0x80200 | 0x80300 | 0x80400 |
| ARQ (events) | 0x80080 / 0x80180 | 0x80280 | 0x80380 | 0x80480 |

The length register holds the descriptor count with bit 31 = enable.
Setup: everything zero first (a previous driver's rings may still be
programmed), head = tail = 0, length | enable, base; the base must read back.
The ARQ is given all its buffers and tail = count - 1.

Descriptor: flags (0-1), opcode (2-3), data length (4-5), return value (6-7),
cookie (8-15), parameters (16-31). Flags: DD 0x1, CMP 0x2, ERR 0x4, LB 0x200
(buffer over 512 bytes), RD 0x400 (firmware reads the buffer), BUF 0x1000
(indirect: buffer address in parameter bytes 8-11 high, 12-15 low), SI
0x2000 (set on every command). A command is done when the head register
reaches the tail (poll 50 µs, give up at 250 ms); the descriptor is written
back with DD, and ERR plus a code in the return value on failure (13 =
already exists, treated as success where noted).

Commands used (parameter offsets are into the 16 parameter bytes):

| Opcode | Command | Use |
|---|---|---|
| 0x0001 | Get Version | ROM 0-3, build 4-7, FW major/minor 8/10, API major/minor 12/14; API major must be 1 |
| 0x0003 | Queue Shutdown | bytes 0-3 = 1: driver unloading (teardown) |
| 0x0107 | MAC Address Read | indirect, 24 bytes: LAN MAC at 0; flags 0-1, 0x10 = LAN address valid |
| 0x0110 | Clear PXE Mode | byte 0 = 2; then GLLAN_RCTL_0 (0x12A500) = 1; "exists" = already clear |
| 0x0200 | Get Switch Config | indirect; bytes 0-1: SEID to start at, written back as the next one (0: done); buffer: 16-byte header (count at 0), 16-byte elements (type at 0, 19 = VSI; SEID at 2) |
| 0x0212 | Get VSI Parameters | bytes 0-1 SEID; 128-byte buffer: queue_mapping[0] at 30 (first queue), qs_handle[0] at 96 (queue set) |
| 0x0250 | Add MAC/VLAN | count 0-1, SEID 2-3 with bit 15 valid; element: MAC 0-5, flags 8-9 (perfect 0x1, ignore VLAN 0x4) |
| 0x0254 | Set VSI Promiscuous Modes | flags 0-1, valid 2-3 (unicast 0x1, multicast 0x2, broadcast 0x4, receive-only 0x8000 from API 1.5), SEID 4-5 |
| 0x0605 | Restart AN | byte 0: restart 0x2, link enable 0x4 |
| 0x0607 | Get Link Status | bytes 0-1: 3 = report changes as ARQ events; answer: PHY type 2, speed 3 (0x02 100M, 0x04 1G, 0x08 10G, 0x10 40G, 0x20 20G, 0x40 25G, 0x01 2.5G, 0x80 5G), link info 4 (bit 0 up), AN info 5 |

Link events arrive on the ARQ with opcode 0x0607 and the same parameters.
Software reads descriptors up to the ARQ head, re-arms each (BUF, length,
buffer address) and sets the tail to the last one it read.

## 4. HMC (queue contexts)

The device keeps queue contexts in host memory, in the function's private
memory space (FPM):

- GLHMC_LANQMAX (0xC2008): most queues; GLHMC_LANTXOBJSZ (0xC2004) and
  GLHMC_LANRXOBJSZ (0xC200C): log2 of the object sizes (128 and 32 bytes).
- Per PF `n`: GLHMC_LANTXBASE (0xC6200 + 4n), LANTXCNT (0xC6300), LANRXBASE
  (0xC6400), LANRXCNT (0xC6500), FCOEDDPBASE/CNT (0xC6600/0xC6700),
  FCOEFBASE/CNT (0xC6800/0xC6900). Bases are in 512-byte units.
- Segment descriptors cover 2 MiB of FPM each: write PFHMC_SDDATAHIGH
  (0xC0200) = address high, PFHMC_SDDATALOW (0xC0100) = address low | backing
  page count (512) << 2 | type (bit 1: 1 direct, 0 paged) | valid (bit 0),
  then PFHMC_SDCMD (0xC0000) = index | bit 31 (write). PFHMC_PDINV (0xC0300)
  = SD index | PD index << 16 invalidates a cached page descriptor.
- A paged SD points to a 4 KiB page of 512 eight-byte page descriptors, each
  a 4 KiB page address | 1 (valid).

The driver uses one paged SD (index 0), one PD, one backing page: transmit
contexts at FPM 0, receive contexts at the next 512-byte boundary, enough of
each for the VSI's first queue.

Transmit context (128 bytes; bit positions): head 0/13, new context 30/1,
base (128-byte units) 32/57, flow control 89/1, head write-back enable
160/1, ring length 161/13, head write-back address 192/64, queue set
("rdylist") 980/10.

Receive context (32 bytes): head 0/13, base (128-byte units) 32/57, ring
length 89/13, data buffer size (128-byte units) 102/7, header buffer 109/5,
descriptor type 114/2 (0: one buffer), descriptor size 116/1 (1: 32 bytes),
CRC strip 117/1, flow control 118/1, L2 tag select 119/1, header split
120/4, show IV 127/1, largest frame 174/14, prefetch 201/1.

## 5. Queues

| Register | Offset (q = PF queue) |
|---|---|
| QTX_ENA / QRX_ENA | 0x100000 + 4q / 0x120000 + 4q: request bit 0, status bit 2 |
| QTX_CTL | 0x104000 + 4q: type 2 (PF queue) in 1:0, PF index in 5:2 |
| QTX_TAIL / QRX_TAIL | 0x108000 + 4q / 0x128000 + 4q |
| GLLAN_TXPRE_QDIS | 0xE6500 + 4 x (abs / 128): queue abs % 128 in 10:0, set-disable 30, clear-disable 31 |
| QINT_RQCTL / QINT_TQCTL | 0x3A000 + 4q / 0x3C000 + 4q: ITR index 12:11, next queue 26:16, next type 28:27 (0 RX, 1 TX), cause enable 30 |
| PFINT_ITR0(i) | 0x38000 + 128 i: interval in 2 µs units |

Start: contexts written, QTX_CTL set, receive queue enabled, transmit
pre-disable cleared, transmit queue enabled (each enable waits up to 100 ms
for its status bit), receive tail = 24. Stop: transmit pre-disable set,
400 µs, both queues disabled (status waited for).

**Write-back.** The device writes descriptors back a cache line (four) at a
time unless something flushes them. With no interrupts in use, the driver
links the queue pair to vector 0 (PFINT_LNKLST0 = receive queue, RQCTL ->
TQCTL -> end, causes enabled, ITR0 = 16 µs) and sets PFINT_DYN_CTL0
WB_ON_ITR (bit 30) with the interrupt itself (bit 0) clear: each ITR expiry
writes back whatever is done, and no interrupt is raised.

**Descriptors.** Receive (32 bytes): packet buffer address at 0, header
buffer address at 8 (0), the rest 0; written back with quadword 1 holding
status (DD bit 0, EOF bit 1), errors from bit 19 (receive error 19, oversize
25), length in bits 51:38. Transmit data (16 bytes): buffer address at 0;
quadword 1: type 3:0 (0), command 13:4 (EOP 0x1, RS 0x2, insert CRC 0x4),
buffer size 47:34. The device sets the type to 0xF when it has sent a
descriptor with RS. The receive tail is moved in steps of eight.

## 6. Teardown

Queues stopped, Queue Shutdown (best effort), both admin queues disabled
and their registers zeroed, the SD invalidated, PF reset. Only then is the
DMA region freed (binding Stop) or the OS let in (ExitBootServices).
