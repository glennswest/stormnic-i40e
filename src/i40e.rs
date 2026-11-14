//! The Intel 700-series physical function (X710, XL710, XXV710, X722),
//! independent of UEFI so every sequence can be tested against a simulated
//! device (`test/sim.rs`).
//!
//! Hardware facts (register offsets and fields, the reset sequence, the
//! admin-queue protocol and commands, the HMC and queue-context layouts, the
//! descriptor formats) are from Intel's BSD-3-Clause shared code in FreeBSD
//! `sys/dev/ixl` (commit 4b732459f030); docs/hardware.md says them in its
//! own words. The code is written here, not translated. What this driver
//! deliberately does not do:
//!
//! - no interrupts (the SNP polls; the queues' interrupt cause is linked to
//!   vector 0 only so that write-back-on-ITR flushes descriptors, with the
//!   interrupt itself never enabled), no RSS, one queue pair, no offloads,
//!   no VLAN, no jumbo frames;
//! - no NVM writes; the only PHY writes are the port's FEC, set to the IEEE
//!   802.3 standard for its media (`port`, the stormnic contract,
//!   stormnic-i40e#5) when the port does not already run it, and "restart
//!   autonegotiation with the link enabled" when the link is down;
//! - no flow control: nothing here turns 802.3x pause on.
//!
//! Every sub-module works on one DMA region (`Dma`, laid out below) that the
//! caller allocated and mapped as a PciIo common buffer.

// The register, field and descriptor definitions are kept complete (they
// document the hardware and the simulator in test/ uses them); the driver
// itself does not use every one.
#![allow(dead_code)]

#[path = "barrier.rs"]
pub mod barrier;
#[path = "adminq.rs"]
pub mod adminq;
#[path = "hmc.rs"]
pub mod hmc;
#[path = "ring.rs"]
pub mod ring;
#[path = "snp_core.rs"]
pub mod snp;
#[path = "port.rs"]
pub mod port;

use core::ptr;

// Registers (memory BAR 0).
pub const GLGEN_RSTCTL: u32 = 0x000b_8180;
pub const GLGEN_RSTAT: u32 = 0x000b_8188;
pub const GLNVM_ULD: u32 = 0x000b_6008;
pub const PFGEN_CTRL: u32 = 0x0009_2400;
pub const PF_FUNC_RID: u32 = 0x0009_c000;
pub const PFGEN_PORTNUM: u32 = 0x001c_0480;
pub const PFLAN_QALLOC: u32 = 0x001c_0400;
pub const GLLAN_RCTL_0: u32 = 0x0012_a500;
pub const PFINT_ICR0_ENA: u32 = 0x0003_8800;
pub const PFINT_DYN_CTL0: u32 = 0x0003_8480;
pub const PFINT_LNKLST0: u32 = 0x0003_8500;
/// ITR interval register `i` (0..=2) of vector 0.
pub const fn pfint_itr0(i: u32) -> u32 { 0x0003_8000 + i * 128 }
pub const fn gllan_txpre_qdis(block: u32) -> u32 { 0x000e_6500 + block * 4 }
// Per-queue registers, indexed by the PF-relative queue number.
pub const fn qtx_ena(q: u32) -> u32 { 0x0010_0000 + q * 4 }
pub const fn qrx_ena(q: u32) -> u32 { 0x0012_0000 + q * 4 }
pub const fn qtx_ctl(q: u32) -> u32 { 0x0010_4000 + q * 4 }
pub const fn qtx_tail(q: u32) -> u32 { 0x0010_8000 + q * 4 }
pub const fn qrx_tail(q: u32) -> u32 { 0x0012_8000 + q * 4 }
pub const fn qtx_head(q: u32) -> u32 { 0x000e_4000 + q * 4 }
pub const fn qint_rqctl(q: u32) -> u32 { 0x0003_a000 + q * 4 }
pub const fn qint_tqctl(q: u32) -> u32 { 0x0003_c000 + q * 4 }

// GLGEN_RSTCTL / GLGEN_RSTAT / GLNVM_ULD / PFGEN_CTRL.
pub const RSTCTL_GRSTDEL_MASK: u32 = 0x3f;
pub const RSTAT_DEVSTATE_MASK: u32 = 0x3;
pub const ULD_CORE_DONE: u32 = 1 << 3;
pub const ULD_GLOBAL_DONE: u32 = 1 << 4;
pub const PFGEN_CTRL_PFSWR: u32 = 1 << 0;
// PFLAN_QALLOC.
pub const QALLOC_FIRSTQ_MASK: u32 = 0x7ff;
pub const QALLOC_LASTQ_SHIFT: u32 = 16;
pub const QALLOC_VALID: u32 = 1 << 31;
// GLLAN_TXPRE_QDIS.
pub const TXPRE_QINDX_MASK: u32 = 0x7ff;
pub const TXPRE_SET_QDIS: u32 = 1 << 30;
pub const TXPRE_CLEAR_QDIS: u32 = 1 << 31;
// QTX_ENA / QRX_ENA.
pub const QENA_REQ: u32 = 1 << 0;
pub const QENA_STAT: u32 = 1 << 2;
// QTX_CTL: the queue belongs to a PF (type 2), PF index at bit 2.
pub const QTX_CTL_PF_QUEUE: u32 = 2;
pub const QTX_CTL_PF_INDX_SHIFT: u32 = 2;
// PFINT_DYN_CTL0.
pub const DYN_CTL0_ITR_INDX_NONE: u32 = 3 << 3;
pub const DYN_CTL0_WB_ON_ITR: u32 = 1 << 30;
// PFINT_LNKLST0 / QINT_[RT]QCTL.
pub const LNKLST_EOL: u32 = 0x7ff;
pub const QTYPE_RX: u32 = 0;
pub const QTYPE_TX: u32 = 1;
pub const LNKLST0_FIRSTQ_TYPE_SHIFT: u32 = 11;
pub const QINT_NEXTQ_INDX_SHIFT: u32 = 16;
pub const QINT_NEXTQ_TYPE_SHIFT: u32 = 27;
pub const QINT_CAUSE_ENA: u32 = 1 << 30;
/// ITR0 of vector 0, in 2 µs units: write-back at most 16 µs late.
pub const ITR_WRITEBACK: u32 = 8;

/// How many queues `clear` disables at most (the PF's allocation is
/// usually far smaller; the hardware has 1536).
pub const MAX_PF_QUEUES: u32 = 1536;

/// The memory BAR's registers and the firmware's Stall.
pub trait Bus {
    type Error: Copy + core::fmt::Debug;
    fn r32(&mut self, offset: u32) -> Result<u32, Self::Error>;
    fn w32(&mut self, offset: u32, value: u32) -> Result<(), Self::Error>;
    fn delay_us(&mut self, micros: usize);
}

/// Which wait of the reset sequence gave up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetStage {
    /// GLGEN_RSTAT.DEVSTATE: a global (core/EMP) reset still in progress.
    Global,
    /// GLNVM_ULD: the firmware has not finished loading the core and global
    /// modules.
    Firmware,
    /// PFGEN_CTRL.PFSWR did not clear.
    Pf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error<E> {
    Io(E),
    /// A register read all ones: the function is gone (or its BAR is not decoded).
    Removed,
    /// A reset wait gave up; the last value of the register it polled.
    Reset { stage: ResetStage, value: u32 },
    /// PFLAN_QALLOC says this PF has no LAN queues.
    NoQueues { qalloc: u32 },
    /// The admin queue's base register did not read back what was written.
    AdminSetup { bal: u32 },
    /// The firmware did not take the command within 250 ms (ATQH never
    /// reached ATQT), or took it without writing it back (DD clear).
    AdminTimeout { opcode: u16, head: u32 },
    /// The firmware answered the command with an error (`retval`, an
    /// `i40e_admin_queue_err` code).
    Admin { opcode: u16, retval: u16 },
    /// The firmware's admin-queue API major version is not 1.
    AdminApi { major: u16, minor: u16 },
    /// The firmware has no valid LAN MAC address for this PF.
    NoMac,
    /// The switch configuration lists no VSI.
    NoVsi,
    /// The VSI's first queue is beyond what one HMC backing page holds.
    QueueRange { queue: u16 },
    /// The HMC cannot hold the queue contexts (object sizes or LANQMAX).
    Hmc { qmax: u32, tx_size: u32, rx_size: u32 },
    /// The DMA region's device address is not 4 KiB aligned.
    Unaligned { device: u64 },
    /// A queue's enable status did not follow the request within 100 ms.
    QueueTimeout { tx: bool, value: u32 },
    /// A frame outside 14..=1518 bytes to send.
    Frame { len: usize },
}

impl<E> From<E> for Error<E> {
    fn from(e: E) -> Self { Error::Io(e) }
}

pub type R<T, E> = Result<T, Error<E>>;

/// A register read that cannot legitimately be all ones.
pub fn rd<B: Bus>(b: &mut B, reg: u32) -> R<u32, B::Error> {
    let v = b.r32(reg)?;
    if v == u32::MAX { return Err(Error::Removed); }
    Ok(v)
}

/// Link state, from the firmware's Get Link Status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Link {
    pub up: bool,
    /// Mb/s, 0 when down or the speed code is unknown.
    pub mbps: u32,
    /// The PHY type (the media: `port::media`), the link-info and AN-info
    /// bytes, and the config byte (the FEC that runs: `Link::fec`).
    pub phy_type: u8,
    pub info: u8,
    pub an: u8,
    pub config: u8,
}

/// Mb/s for a Get Link Status speed code (one bit set).
pub fn speed_mbps(code: u8) -> u32 {
    match code {
        0x01 => 2_500,
        0x02 => 100,
        0x04 => 1_000,
        0x08 => 10_000,
        0x10 => 40_000,
        0x20 => 20_000,
        0x40 => 25_000,
        0x80 => 5_000,
        _ => 0,
    }
}

impl core::fmt::Display for Link {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if !self.up { return f.write_str("down"); }
        match self.mbps {
            0 => f.write_str("up, speed unknown"),
            m if m % 1000 == 0 => write!(f, "up {} Gb/s", m / 1000),
            m => write!(f, "up {m} Mb/s"),
        }
    }
}

/// Which PF this is and which queues it owns: read only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    /// PCI function number (PF_FUNC_RID), the PF index for QTX_CTL and the
    /// HMC function registers.
    pub pf: u32,
    /// Physical port (PFGEN_PORTNUM).
    pub port: u32,
    /// Absolute index of the PF's first LAN queue (PFLAN_QALLOC.FIRSTQ).
    pub base_queue: u32,
    /// How many LAN queues the PF owns.
    pub queues: u32,
}

pub fn identify<B: Bus>(b: &mut B) -> R<Identity, B::Error> {
    let rid = rd(b, PF_FUNC_RID)?;
    let port = rd(b, PFGEN_PORTNUM)? & 0x3;
    let qalloc = rd(b, PFLAN_QALLOC)?;
    let first = qalloc & QALLOC_FIRSTQ_MASK;
    let last = (qalloc >> QALLOC_LASTQ_SHIFT) & QALLOC_FIRSTQ_MASK;
    if qalloc & QALLOC_VALID == 0 || last < first {
        return Err(Error::NoQueues { qalloc });
    }
    Ok(Identity { pf: rid & 0x7, port, base_queue: first, queues: last - first + 1 })
}

/// Poll `f` every `step` µs, up to `tries` times; whether it came true.
pub fn wait<B: Bus>(b: &mut B, tries: usize, step: usize,
    mut f: impl FnMut(&mut B) -> R<bool, B::Error>) -> R<bool, B::Error> {
    for _ in 0..tries {
        if f(b)? { return Ok(true); }
        b.delay_us(step);
    }
    f(b)
}

/// Tell the transmit scheduler that absolute queue `abs` is about to be
/// enabled (`enable`) or disabled.
pub fn tx_pre<B: Bus>(b: &mut B, abs: u32, enable: bool) -> R<(), B::Error> {
    let reg = gllan_txpre_qdis(abs / 128);
    let v = b.r32(reg)? & !(TXPRE_QINDX_MASK | TXPRE_SET_QDIS | TXPRE_CLEAR_QDIS);
    let v = v | (abs % 128) | if enable { TXPRE_CLEAR_QDIS } else { TXPRE_SET_QDIS };
    Ok(b.w32(reg, v)?)
}

/// What a previous driver may have left running: vector 0's interrupt
/// causes, and every LAN queue of the PF (transmit pre-disabled first, as
/// the hardware asks). Nothing is waited for beyond the settle delays; the
/// PF reset that follows finishes the job.
pub fn clear<B: Bus>(b: &mut B, id: &Identity) -> R<(), B::Error> {
    b.w32(PFINT_ICR0_ENA, 0)?;
    b.w32(PFINT_DYN_CTL0, DYN_CTL0_ITR_INDX_NONE)?;
    b.w32(PFINT_LNKLST0, LNKLST_EOL)?;
    let n = id.queues.min(MAX_PF_QUEUES);
    for q in 0..n { tx_pre(b, id.base_queue + q, false)?; }
    b.delay_us(400);
    for q in 0..n {
        b.w32(qint_tqctl(q), 0)?;
        b.w32(qtx_ena(q), 0)?;
        b.w32(qint_rqctl(q), 0)?;
        b.w32(qrx_ena(q), 0)?;
    }
    b.delay_us(50);
    Ok(())
}

/// What the PF reset saw, for the trace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Reset {
    /// A global reset was in progress, so no PF reset was needed.
    pub global: bool,
    /// Milliseconds the firmware took to report its modules loaded.
    pub firmware_ms: usize,
    /// Milliseconds PFSWR took to clear.
    pub pf_ms: usize,
}

/// The PF software reset: wait out any global reset (up to GRSTDEL x 2 s,
/// at most 16 s), wait for the firmware's core and global modules
/// (GLNVM_ULD, up to 10 s), then set PFGEN_CTRL.PFSWR and wait for it to
/// clear (up to 1 s; a global reset starting meanwhile is waited out too).
/// The PF's queues, interrupts and HMC segment descriptors are back at their
/// reset values afterwards; the admin queue registers are not (EMP reset only).
pub fn pf_reset<B: Bus>(b: &mut B) -> R<Reset, B::Error> {
    let grstdel = (rd(b, GLGEN_RSTCTL)? & RSTCTL_GRSTDEL_MASK) as usize;
    let global_tries = (grstdel * 20).min(160);
    let mut reset = Reset::default();
    let mut polls = 0;
    let mut rstat = 0;
    while polls < global_tries.max(1) {
        rstat = rd(b, GLGEN_RSTAT)?;
        if rstat & RSTAT_DEVSTATE_MASK == 0 { break; }
        b.delay_us(100_000);
        polls += 1;
    }
    if rstat & RSTAT_DEVSTATE_MASK != 0 {
        return Err(Error::Reset { stage: ResetStage::Global, value: rstat });
    }
    reset.global = polls > 0;
    let want = ULD_CORE_DONE | ULD_GLOBAL_DONE;
    let mut uld = 0;
    let mut loaded = false;
    for ms in 0..1000 {
        uld = rd(b, GLNVM_ULD)?;
        if uld & want == want { reset.firmware_ms = ms * 10; loaded = true; break; }
        b.delay_us(10_000);
    }
    if !loaded {
        return Err(Error::Reset { stage: ResetStage::Firmware, value: uld });
    }
    if reset.global { return Ok(reset); }
    let ctrl = rd(b, PFGEN_CTRL)?;
    b.w32(PFGEN_CTRL, ctrl | PFGEN_CTRL_PFSWR)?;
    let mut ctrl = 0;
    for ms in 0..1000 {
        ctrl = rd(b, PFGEN_CTRL)?;
        if ctrl & PFGEN_CTRL_PFSWR == 0 { reset.pf_ms = ms; return Ok(reset); }
        if rd(b, GLGEN_RSTAT)? & RSTAT_DEVSTATE_MASK != 0 {
            // A global reset took over: wait for it like above.
            let ok = wait(b, global_tries, 100_000, |b| Ok(rd(b, GLGEN_RSTAT)? & RSTAT_DEVSTATE_MASK == 0))?;
            if !ok {
                return Err(Error::Reset { stage: ResetStage::Global, value: rd(b, GLGEN_RSTAT)? });
            }
            reset.global = true;
            return Ok(reset);
        }
        b.delay_us(1000);
    }
    Err(Error::Reset { stage: ResetStage::Pf, value: ctrl })
}

/// Undo everything that lets the device reach the DMA region: tell the
/// firmware the driver is unloading (best effort), turn the admin queue
/// off, unhook the HMC page, and reset the PF (which also stops the
/// queues). Every step runs; Ok when the admin queue is off and the reset
/// completed. The queues should be stopped first where they may be running.
pub fn release<B: Bus>(b: &mut B, aq: &mut adminq::AdminQ) -> R<(), B::Error> {
    if aq.api != (0, 0) { let _ = aq.shutdown(b); }
    let off = aq.disable(b);
    let _ = hmc::teardown(b);
    let reset = pf_reset(b).map(|_| ());
    off.and(reset)
}

/// A step of `bring_up`, for the console's trace.
#[derive(Debug, PartialEq, Eq)]
pub enum Step<E> {
    Reset(Reset),
    Version(adminq::Version),
    /// PXE mode was on and is now cleared (false: it was already off).
    Pxe(bool),
    Vsi { mac: [u8; 6], vsi: adminq::Vsi },
    /// The MAC filter for any VLAN: added, already there, or refused
    /// (not fatal: the firmware's own filter passes untagged frames).
    MacFilter(Result<bool, Error<E>>),
    Hmc(hmc::Layout),
    /// The link as first read, and the port's FEC brought to the standard.
    Fec { link: Link, fec: port::PortFec<E> },
    /// The link, and how long Start waited after setting the FEC or
    /// restarting autonegotiation (None: it was up, nothing restarted).
    Link { link: Link, waited_ms: Option<usize> },
    /// The DMA check: the descriptor came back after this many ms, or not
    /// within 100 ms.
    Check(Option<usize>),
}

/// The device brought up: what the SNP needs.
pub struct Up<E> {
    pub net: ring::Net,
    pub link: Link,
    pub version: adminq::Version,
    pub fec: port::PortFec<E>,
    /// How long Start waited for the link (0: it was up).
    pub waited_ms: usize,
}

/// Start's bring-up, each step reported to `log`: clear what a previous
/// driver left, reset the PF, set up the admin queue and ask the firmware
/// for its version, out of PXE mode, the MAC address and the VSI; add the
/// MAC filter, lay out the HMC, read the link, bring the port's FEC to the
/// standard for its media (`port::port_fec`), wait up to `link_wait_ms` for
/// the link if the FEC was written or the link is down (restarting
/// autonegotiation in the second case) and, when it is up, run
/// the DMA check on the queue pair. The queues are stopped on return; on
/// Err the caller tears down with `release`.
pub fn bring_up<B: Bus>(b: &mut B, aq: &mut adminq::AdminQ, dma: Dma, id: &Identity, link_wait_ms: usize,
    log: &mut dyn FnMut(Step<B::Error>)) -> R<Up<B::Error>, B::Error> {
    clear(b, id)?;
    log(Step::Reset(pf_reset(b)?));
    aq.init(b)?;
    let version = aq.version(b)?;
    log(Step::Version(version));
    log(Step::Pxe(aq.clear_pxe(b)?));
    let mac = aq.mac_read(b)?;
    let seid = aq.find_vsi(b)?;
    let vsi = aq.vsi(b, seid)?;
    log(Step::Vsi { mac, vsi });
    log(Step::MacFilter(aq.add_mac(b, vsi.seid, mac)));
    let layout = hmc::setup(b, &dma, id.pf, vsi.queue as u32)?;
    log(Step::Hmc(layout));
    let mut link = aq.link(b, false)?;
    let fec = port::port_fec(b, aq, &link);
    log(Step::Fec { link, fec });
    let mut waited_ms = None;
    let written = matches!(fec.set, port::FecSet::Written);
    if !link.up || written {
        // Set PHY Config restarted the link already.
        if !written { aq.restart_an(b)?; }
        let mut waited = 0;
        while waited < link_wait_ms {
            b.delay_us(100_000);
            waited += 100;
            link = aq.link(b, false)?;
            if link.up { break; }
        }
        waited_ms = Some(waited);
    }
    log(Step::Link { link, waited_ms });
    let mut net = ring::Net::new(dma, mac, *id, vsi, layout);
    if link.up {
        net.start(b)?;
        let checked = ring::check(b, &mut net);
        net.stop(b)?;
        log(Step::Check(checked?));
    }
    Ok(Up { net, link, version, fec, waited_ms: waited_ms.unwrap_or(0) })
}

pub fn unicast(m: [u8; 6]) -> bool {
    m != [0; 6] && m[0] & 1 == 0
}

// The DMA region: one PciIo common buffer, laid out in 4 KiB pages so the
// HMC pages (which the hardware needs page aligned) are.

/// Admin transmit (command) queue: `ATQ_COUNT` 32-byte descriptors.
pub const ATQ: usize = 0x0000;
pub const ATQ_COUNT: usize = 32;
/// Admin receive (event) queue: `ARQ_COUNT` 32-byte descriptors.
pub const ARQ: usize = 0x0400;
pub const ARQ_COUNT: usize = 8;
/// The one indirect-command buffer (commands are issued one at a time).
pub const ATQ_BUF: usize = 0x1000;
pub const ATQ_BUF_SIZE: usize = 4096;
/// Event buffers, `ARQ_BUF_SIZE` each.
pub const ARQ_BUFS: usize = 0x2000;
pub const ARQ_BUF_SIZE: usize = 512;
/// HMC page-descriptor page (one segment descriptor, paged).
pub const HMC_PD: usize = 0x3000;
/// HMC backing page: the queue contexts live here.
pub const HMC_BP: usize = 0x4000;
/// Receive ring (32-byte descriptors) and transmit ring (16-byte); the
/// hardware wants their bases 128-byte aligned.
pub const RX_RING: usize = 0x5000;
pub const TX_RING: usize = 0x5800;
pub const RX_COUNT: usize = 32;
pub const TX_COUNT: usize = 32;
pub const RX_DESC: usize = 32;
pub const TX_DESC: usize = 16;
pub const BUF_SIZE: usize = 2048;
pub const RX_BUFS: usize = 0x6000;
pub const TX_BUFS: usize = RX_BUFS + RX_COUNT * BUF_SIZE;
pub const DMA_BYTES: usize = TX_BUFS + TX_COUNT * BUF_SIZE;
pub const DMA_PAGES: usize = DMA_BYTES.div_ceil(4096);
const _: () = assert!(ARQ + ARQ_COUNT * 32 <= ATQ_BUF && ATQ + ATQ_COUNT * 32 <= ARQ);
const _: () = assert!(ARQ_BUFS + ARQ_COUNT * ARQ_BUF_SIZE <= HMC_PD);
const _: () = assert!(RX_RING + RX_COUNT * RX_DESC <= TX_RING && TX_RING + TX_COUNT * TX_DESC <= RX_BUFS);

/// The DMA region: `DMA_BYTES` at `host`, which the device reaches at
/// `device`. Every access is volatile and little-endian.
#[derive(Clone, Copy, Debug)]
pub struct Dma {
    pub host: *mut u8,
    pub device: u64,
}

impl Dma {
    pub fn r8(&self, off: usize) -> u8 {
        debug_assert!(off < DMA_BYTES);
        // SAFETY: inside the region (callers use the layout above).
        unsafe { ptr::read_volatile(self.host.add(off)) }
    }
    pub fn w8(&self, off: usize, v: u8) {
        debug_assert!(off < DMA_BYTES);
        // SAFETY: as `r8`.
        unsafe { ptr::write_volatile(self.host.add(off), v) }
    }
    pub fn r16(&self, off: usize) -> u16 {
        // SAFETY: as `r8`; every 16-bit field in the layout is 2-byte aligned.
        u16::from_le(unsafe { ptr::read_volatile(self.host.add(off) as *const u16) })
    }
    pub fn w16(&self, off: usize, v: u16) {
        // SAFETY: as `r16`.
        unsafe { ptr::write_volatile(self.host.add(off) as *mut u16, v.to_le()) }
    }
    pub fn r32(&self, off: usize) -> u32 {
        // SAFETY: as `r8`; 4-byte aligned.
        u32::from_le(unsafe { ptr::read_volatile(self.host.add(off) as *const u32) })
    }
    pub fn w32(&self, off: usize, v: u32) {
        // SAFETY: as `r32`.
        unsafe { ptr::write_volatile(self.host.add(off) as *mut u32, v.to_le()) }
    }
    /// Two 32-bit accesses, low half first.
    pub fn r64(&self, off: usize) -> u64 {
        self.r32(off) as u64 | (self.r32(off + 4) as u64) << 32
    }
    pub fn w64(&self, off: usize, v: u64) {
        self.w32(off, v as u32);
        self.w32(off + 4, (v >> 32) as u32);
    }
    pub fn zero(&self, off: usize, len: usize) {
        for k in 0..len { self.w8(off + k, 0); }
    }
    pub fn copy_in(&self, off: usize, src: &[u8]) {
        for (k, v) in src.iter().enumerate() { self.w8(off + k, *v); }
    }
    pub fn copy_out(&self, off: usize, dst: &mut [u8]) {
        for (k, v) in dst.iter_mut().enumerate() { *v = self.r8(off + k); }
    }
    /// The device address of offset `off`.
    pub fn at(&self, off: usize) -> u64 { self.device + off as u64 }
}
