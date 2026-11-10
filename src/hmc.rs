//! The Host Memory Cache (HMC): where the device keeps the LAN queue
//! contexts, in host memory the driver provides.
//!
//! The function's private memory (FPM) is an address space of objects the
//! driver lays out with the GLHMC_LAN{TX,RX}{BASE,CNT} registers (bases in
//! 512-byte units, objects of 2^GLHMC_LAN{TX,RX}OBJSZ bytes: 128 for a
//! transmit context, 32 for a receive one). The FPM is backed by host pages
//! through segment descriptors (SD, 2 MiB of FPM each, programmed through
//! PFHMC_SDCMD/SDDATALOW/SDDATAHIGH). A paged SD points at a page of 512
//! page descriptors (PD, 8 bytes: a 4 KiB page's address with bit 0 = valid).
//!
//! This driver needs one queue pair, so: transmit contexts at FPM 0, receive
//! contexts at the next 512-byte boundary, one paged SD (index 0) whose PD
//! page (`HMC_PD`) has one valid entry, the backing page `HMC_BP`. A
//! context is written straight into that page by its bit layout; the device
//! reads it when the queue is enabled.

use super::{rd, Bus, Dma, Error, R, HMC_BP, HMC_PD};

pub const GLHMC_LANTXOBJSZ: u32 = 0x000c_2004;
pub const GLHMC_LANQMAX: u32 = 0x000c_2008;
pub const GLHMC_LANRXOBJSZ: u32 = 0x000c_200c;
pub const fn glhmc_lantxbase(pf: u32) -> u32 { 0x000c_6200 + pf * 4 }
pub const fn glhmc_lantxcnt(pf: u32) -> u32 { 0x000c_6300 + pf * 4 }
pub const fn glhmc_lanrxbase(pf: u32) -> u32 { 0x000c_6400 + pf * 4 }
pub const fn glhmc_lanrxcnt(pf: u32) -> u32 { 0x000c_6500 + pf * 4 }
pub const fn glhmc_fcoeddpbase(pf: u32) -> u32 { 0x000c_6600 + pf * 4 }
pub const fn glhmc_fcoeddpcnt(pf: u32) -> u32 { 0x000c_6700 + pf * 4 }
pub const fn glhmc_fcoefbase(pf: u32) -> u32 { 0x000c_6800 + pf * 4 }
pub const fn glhmc_fcoefcnt(pf: u32) -> u32 { 0x000c_6900 + pf * 4 }
pub const PFHMC_SDCMD: u32 = 0x000c_0000;
pub const PFHMC_SDDATALOW: u32 = 0x000c_0100;
pub const PFHMC_SDDATAHIGH: u32 = 0x000c_0200;
pub const PFHMC_PDINV: u32 = 0x000c_0300;
pub const PFHMC_ERRORINFO: u32 = 0x000c_0400;

pub const SDCMD_WRITE: u32 = 1 << 31;
pub const SD_VALID: u32 = 1 << 0;
/// SDDATALOW.PMSDTYPE: 1 direct, 0 paged.
pub const SD_DIRECT: u32 = 1 << 1;
pub const SD_BPCOUNT_SHIFT: u32 = 2;
/// Page descriptors in a paged SD.
pub const PD_COUNT: u32 = 512;
pub const PDINV_PDIDX_SHIFT: u32 = 16;
pub const PD_VALID: u64 = 1;
pub const OBJ_ALIGN: u64 = 512;
pub const PAGE: u64 = 4096;

/// Where the contexts are in the FPM (and so in the backing page).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub tx_size: u64,
    pub rx_base: u64,
    pub rx_size: u64,
    /// Objects of each kind: enough for the VSI's queue index.
    pub count: u32,
}

impl Layout {
    pub fn tx(&self, queue: u32) -> usize { (queue as u64 * self.tx_size) as usize }
    pub fn rx(&self, queue: u32) -> usize { (self.rx_base + queue as u64 * self.rx_size) as usize }
}

fn align(v: u64) -> u64 { v.div_ceil(OBJ_ALIGN) * OBJ_ALIGN }

/// Lay out contexts for queues 0..=`queue` and program the FPM registers
/// and the segment descriptor.
pub fn setup<B: Bus>(b: &mut B, dma: &Dma, pf: u32, queue: u32) -> R<Layout, B::Error> {
    if dma.at(0) % PAGE != 0 { return Err(Error::Unaligned { device: dma.at(0) }); }
    let qmax = rd(b, GLHMC_LANQMAX)? & 0x7ff;
    let tx_exp = rd(b, GLHMC_LANTXOBJSZ)? & 0xf;
    let rx_exp = rd(b, GLHMC_LANRXOBJSZ)? & 0xf;
    let (tx_size, rx_size) = (1u64 << tx_exp, 1u64 << rx_exp);
    let count = queue + 1;
    let rx_base = align(count as u64 * tx_size);
    let end = align(rx_base + count as u64 * rx_size);
    let hmc = Error::Hmc { qmax, tx_size: tx_size as u32, rx_size: rx_size as u32 };
    // The contexts this driver writes are 128 and 32 bytes; smaller objects
    // could not hold them, and everything must fit the one backing page.
    if count > qmax || tx_size < 128 || rx_size < 32 { return Err(hmc); }
    if end > PAGE { return Err(Error::QueueRange { queue: queue as u16 }); }
    dma.zero(HMC_PD, PAGE as usize);
    dma.zero(HMC_BP, PAGE as usize);
    dma.w64(HMC_PD, dma.at(HMC_BP) | PD_VALID);
    super::barrier::dma_wmb();
    super::barrier::mmio_wmb();
    b.w32(glhmc_lantxbase(pf), 0)?;
    b.w32(glhmc_lantxcnt(pf), count)?;
    b.w32(glhmc_lanrxbase(pf), (rx_base / OBJ_ALIGN) as u32)?;
    b.w32(glhmc_lanrxcnt(pf), count)?;
    // No FCoE objects: zero of each, after the receive contexts.
    b.w32(glhmc_fcoeddpbase(pf), (end / OBJ_ALIGN) as u32)?;
    b.w32(glhmc_fcoeddpcnt(pf), 0)?;
    b.w32(glhmc_fcoefbase(pf), (end / OBJ_ALIGN) as u32)?;
    b.w32(glhmc_fcoefcnt(pf), 0)?;
    let pd = dma.at(HMC_PD);
    b.w32(PFHMC_SDDATAHIGH, (pd >> 32) as u32)?;
    b.w32(PFHMC_SDDATALOW, pd as u32 | PD_COUNT << SD_BPCOUNT_SHIFT | SD_VALID)?;
    b.w32(PFHMC_SDCMD, SDCMD_WRITE)?;
    b.w32(PFHMC_PDINV, 0)?;
    Ok(Layout { tx_size, rx_base, rx_size, count })
}

/// Invalidate the segment descriptor: the device no longer reaches the
/// HMC pages. The PF reset that follows at teardown clears it too.
pub fn teardown<B: Bus>(b: &mut B) -> R<(), B::Error> {
    b.w32(PFHMC_SDDATAHIGH, 0)?;
    b.w32(PFHMC_SDDATALOW, PD_COUNT << SD_BPCOUNT_SHIFT)?;
    b.w32(PFHMC_SDCMD, SDCMD_WRITE)?;
    b.w32(PFHMC_PDINV, 0)?;
    Ok(())
}

/// Put `value`'s low `width` bits at bit `lsb` of `ctx` (little-endian bit
/// numbering: bit n is bit n % 8 of byte n / 8).
pub fn put(ctx: &mut [u8], lsb: usize, width: usize, value: u64) {
    for i in 0..width {
        let bit = lsb + i;
        let mask = 1u8 << (bit % 8);
        if (value >> i) & 1 != 0 { ctx[bit / 8] |= mask; } else { ctx[bit / 8] &= !mask; }
    }
}

/// Read `width` bits at `lsb` (for the simulator and the tests).
pub fn get(ctx: &[u8], lsb: usize, width: usize) -> u64 {
    (0..width).fold(0, |v, i| {
        let bit = lsb + i;
        v | (((ctx[bit / 8] >> (bit % 8)) & 1) as u64) << i
    })
}

/// LAN transmit queue context fields (bit position, width).
pub mod tx {
    pub const HEAD: (usize, usize) = (0, 13);
    pub const NEW_CONTEXT: (usize, usize) = (30, 1);
    /// Ring base address in 128-byte units.
    pub const BASE: (usize, usize) = (32, 57);
    pub const FC_ENA: (usize, usize) = (89, 1);
    pub const HEAD_WB_ENA: (usize, usize) = (128 + 32, 1);
    /// Ring length in descriptors.
    pub const QLEN: (usize, usize) = (128 + 33, 13);
    pub const HEAD_WB_ADDR: (usize, usize) = (128 + 64, 64);
    /// The queue set this queue's traffic is scheduled in.
    pub const RDYLIST: (usize, usize) = (7 * 128 + 84, 10);
    pub const SIZE: usize = 128;
}

/// LAN receive queue context fields (bit position, width).
pub mod rx {
    pub const HEAD: (usize, usize) = (0, 13);
    /// Ring base address in 128-byte units.
    pub const BASE: (usize, usize) = (32, 57);
    pub const QLEN: (usize, usize) = (89, 13);
    /// Data buffer size in 128-byte units.
    pub const DBUFF: (usize, usize) = (102, 7);
    pub const HBUFF: (usize, usize) = (109, 5);
    /// Descriptor type: 0, one buffer, no header split.
    pub const DTYPE: (usize, usize) = (114, 2);
    /// Descriptor size: 0 16 bytes, 1 32 bytes.
    pub const DSIZE: (usize, usize) = (116, 1);
    pub const CRCSTRIP: (usize, usize) = (117, 1);
    pub const FC_ENA: (usize, usize) = (118, 1);
    /// Where a stripped VLAN tag is reported (1: L2TAG1 of the write-back).
    pub const L2TSEL: (usize, usize) = (119, 1);
    pub const HSPLIT_0: (usize, usize) = (120, 4);
    pub const SHOWIV: (usize, usize) = (127, 1);
    /// Largest frame the queue takes, in bytes.
    pub const RXMAX: (usize, usize) = (174, 14);
    pub const PREFENA: (usize, usize) = (201, 1);
    pub const SIZE: usize = 32;
}

fn set(ctx: &mut [u8], f: (usize, usize), v: u64) { put(ctx, f.0, f.1, v); }

/// The transmit context for a ring of `count` descriptors at `ring`.
pub fn tx_context(ring: u64, count: usize, qset: u16) -> [u8; tx::SIZE] {
    let mut c = [0u8; tx::SIZE];
    set(&mut c, tx::NEW_CONTEXT, 1);
    set(&mut c, tx::BASE, ring / 128);
    set(&mut c, tx::QLEN, count as u64);
    set(&mut c, tx::RDYLIST, qset as u64);
    c
}

/// The receive context: 32-byte descriptors, one `buf`-byte buffer each,
/// CRC stripped, frames up to `rxmax` bytes, descriptor prefetch on.
pub fn rx_context(ring: u64, count: usize, buf: usize, rxmax: usize) -> [u8; rx::SIZE] {
    let mut c = [0u8; rx::SIZE];
    set(&mut c, rx::BASE, ring / 128);
    set(&mut c, rx::QLEN, count as u64);
    set(&mut c, rx::DBUFF, (buf / 128) as u64);
    set(&mut c, rx::DSIZE, 1);
    set(&mut c, rx::CRCSTRIP, 1);
    set(&mut c, rx::L2TSEL, 1);
    set(&mut c, rx::RXMAX, rxmax as u64);
    set(&mut c, rx::PREFENA, 1);
    c
}

/// Write queue `queue`'s two contexts into the backing page.
pub fn write_contexts(dma: &Dma, l: &Layout, queue: u32, tx: &[u8; tx::SIZE], rx: &[u8; rx::SIZE]) {
    dma.zero(HMC_BP + l.tx(queue), l.tx_size as usize);
    dma.copy_in(HMC_BP + l.tx(queue), tx);
    dma.zero(HMC_BP + l.rx(queue), l.rx_size as usize);
    dma.copy_in(HMC_BP + l.rx(queue), rx);
    super::barrier::dma_wmb();
}
