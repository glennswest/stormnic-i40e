//! The VSI's first queue pair, polled, one 2 KiB driver-owned buffer per
//! descriptor.
//!
//! Descriptor formats (docs/hardware.md), little-endian:
//!
//! - receive, 32 bytes (context DSIZE 1): read format packet buffer address
//!   (0-7), header buffer address (8-15, 0: no split; its bit 0 is where DD
//!   lands), 16-31 zero; write-back has, in quadword 1 (8-15), status bits
//!   0-18 (DD 0, EOF 1), error bits 19-26 (RXE 19, oversize 25; the others
//!   are checksum results) and the packet length in bits 38-51;
//! - transmit data, 16 bytes: buffer address (0-7), and quadword 1 with the
//!   descriptor type in bits 0-3 (0 data), the command in 4-13 (EOP, RS,
//!   ICRC), the buffer size in 34-47. The device writes type 0xF (done)
//!   back into a descriptor with RS once it has sent it.
//!
//! Ownership moves by the tail registers: the device owns the receive
//! descriptors from its head up to QRX_TAIL - 1 and the transmit ones up to
//! QTX_TAIL - 1. Receive buffers go back in batches of 8 (the tail moves
//! only to multiples of 8), so the device always holds 24 to 31 of them.
//!
//! Descriptors are written back in whole cache lines unless something
//! flushes them; with no interrupts, write-back-on-ITR (PFINT_DYN_CTL0
//! WB_ON_ITR, the interrupt itself left disabled) is that something: the
//! queue pair's interrupt cause is linked to vector 0, whose ITR0 expires
//! every 16 µs and flushes.

use super::barrier::{dma_rmb, dma_wmb, mmio_wmb};
use super::hmc::{self, Layout};
use super::*;

/// Transmit quadword-1 fields.
pub const TX_DTYPE_MASK: u64 = 0xf;
pub const TX_DTYPE_DONE: u64 = 0xf;
pub const TX_CMD_SHIFT: u32 = 4;
pub const TX_CMD_EOP: u64 = 0x1;
pub const TX_CMD_RS: u64 = 0x2;
pub const TX_CMD_ICRC: u64 = 0x4;
pub const TX_BUF_SZ_SHIFT: u32 = 34;
/// Receive quadword-1 fields.
pub const RX_DD: u64 = 1 << 0;
pub const RX_EOF: u64 = 1 << 1;
pub const RX_ERROR_SHIFT: u32 = 19;
pub const RX_ERR_RXE: u64 = 1 << 19;
pub const RX_ERR_OVERSIZE: u64 = 1 << 25;
pub const RX_LEN_SHIFT: u32 = 38;
pub const RX_LEN_MASK: u64 = 0x3fff;

/// Largest frame without CRC: 1514 plus a 4-byte VLAN tag.
pub const MAX_FRAME: usize = 1518;
/// The receive context's limit, with the CRC.
pub const RX_MAX: usize = MAX_FRAME + 4;
/// Ethernet header: the smallest frame the driver will send.
pub const MIN_FRAME: usize = 14;
/// Short frames are padded to this (64 bytes on the wire with the CRC).
pub const PAD_TO: usize = 60;
/// Receive tail moves in steps of this many descriptors.
pub const RX_BATCH: usize = 8;

/// A frame's Ethernet header fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    pub destination: [u8; 6],
    pub source: [u8; 6],
    pub ethertype: u16,
}

pub fn frame(f: &[u8]) -> Frame {
    let mut d = [0u8; 6];
    let mut s = [0u8; 6];
    d.copy_from_slice(&f[0..6]);
    s.copy_from_slice(&f[6..12]);
    Frame { destination: d, source: s, ethertype: u16::from_be_bytes([f[12], f[13]]) }
}

pub struct Net {
    pub dma: Dma,
    pub mac: [u8; 6],
    pub id: Identity,
    pub vsi: adminq::Vsi,
    pub hmc: Layout,
    /// The receive descriptor to look at next.
    rx_next: usize,
    /// Transmit descriptors handed to the device and not yet seen back:
    /// `tx_cons` up to (not including) `tx_prod`, counted modulo 2^usize.
    tx_prod: usize,
    tx_cons: usize,
    pub running: bool,
    /// Receive descriptors that came back with an error or a split frame,
    /// since `start`.
    pub rx_errors: usize,
}

impl Net {
    pub fn new(dma: Dma, mac: [u8; 6], id: Identity, vsi: adminq::Vsi, hmc: Layout) -> Net {
        Net { dma, mac, id, vsi, hmc, rx_next: 0, tx_prod: 0, tx_cons: 0, running: false, rx_errors: 0 }
    }

    fn q(&self) -> u32 { self.vsi.queue as u32 }
    fn rx_desc(i: usize) -> usize { RX_RING + i * RX_DESC }
    fn tx_desc(i: usize) -> usize { TX_RING + i * TX_DESC }

    /// Point receive descriptor `i` at its buffer; everything else (and
    /// with it DD) zero.
    fn post_rx(&self, i: usize) {
        let d = Self::rx_desc(i);
        self.dma.w64(d, self.dma.at(RX_BUFS + i * BUF_SIZE));
        self.dma.w64(d + 8, 0);
        self.dma.w64(d + 16, 0);
        self.dma.w64(d + 24, 0);
    }

    /// Program both contexts, link the queue pair to vector 0 for
    /// write-back, enable the receive queue then the transmit queue, and
    /// hand the device its receive buffers. On failure the queues are
    /// disabled again and nothing runs.
    pub fn start<B: Bus>(&mut self, b: &mut B) -> R<(), B::Error> {
        self.running = false;
        let r = self.bring_up(b);
        if r.is_err() { let _ = self.stop(b); }
        r
    }

    fn bring_up<B: Bus>(&mut self, b: &mut B) -> R<(), B::Error> {
        let q = self.q();
        self.dma.zero(RX_RING, RX_COUNT * RX_DESC);
        self.dma.zero(TX_RING, TX_COUNT * TX_DESC);
        for i in 0..RX_COUNT { self.post_rx(i); }
        self.rx_next = 0;
        self.tx_prod = 0;
        self.tx_cons = 0;
        self.rx_errors = 0;
        let tx = hmc::tx_context(self.dma.at(TX_RING), TX_COUNT, self.vsi.qset);
        let rx = hmc::rx_context(self.dma.at(RX_RING), RX_COUNT, BUF_SIZE, RX_MAX);
        hmc::write_contexts(&self.dma, &self.hmc, q, &tx, &rx);
        mmio_wmb();
        b.w32(qtx_ctl(q), QTX_CTL_PF_QUEUE | self.id.pf << QTX_CTL_PF_INDX_SHIFT)?;
        // Write-back on ITR: vector 0's list is RX q -> TX q -> end.
        b.w32(pfint_itr0(0), ITR_WRITEBACK)?;
        b.w32(qint_rqctl(q), QINT_CAUSE_ENA | q << QINT_NEXTQ_INDX_SHIFT | QTYPE_TX << QINT_NEXTQ_TYPE_SHIFT)?;
        b.w32(qint_tqctl(q), QINT_CAUSE_ENA | LNKLST_EOL << QINT_NEXTQ_INDX_SHIFT)?;
        // FIRSTQ_TYPE 0 (QTYPE_RX): the list starts at the receive queue.
        b.w32(PFINT_LNKLST0, q)?;
        b.w32(PFINT_DYN_CTL0, DYN_CTL0_WB_ON_ITR | DYN_CTL0_ITR_INDX_NONE)?;
        b.w32(qrx_tail(q), 0)?;
        enable(b, qrx_ena(q), false)?;
        tx_pre(b, self.id.base_queue + q, true)?;
        b.w32(qtx_tail(q), 0)?;
        enable(b, qtx_ena(q), true)?;
        dma_wmb();
        mmio_wmb();
        b.w32(qrx_tail(q), (RX_COUNT - RX_BATCH) as u32)?;
        self.running = true;
        Ok(())
    }

    /// Disable the transmit queue (scheduler told first) then the receive
    /// queue, and unlink them from vector 0. Ok: the device does no queue
    /// DMA any more.
    pub fn stop<B: Bus>(&mut self, b: &mut B) -> R<(), B::Error> {
        self.running = false;
        let q = self.q();
        tx_pre(b, self.id.base_queue + q, false)?;
        b.delay_us(400);
        let t = disable(b, qtx_ena(q), true);
        let r = disable(b, qrx_ena(q), false);
        b.w32(PFINT_DYN_CTL0, DYN_CTL0_ITR_INDX_NONE)?;
        b.w32(PFINT_LNKLST0, LNKLST_EOL)?;
        b.w32(qint_rqctl(q), 0)?;
        b.w32(qint_tqctl(q), 0)?;
        t?;
        r
    }

    /// Transmit descriptors the device still owns.
    pub fn tx_pending(&self) -> usize { self.tx_prod.wrapping_sub(self.tx_cons) }

    /// Retire the transmit descriptors the device has marked done.
    pub fn reclaim(&mut self) {
        while self.tx_pending() > 0 {
            let i = self.tx_cons % TX_COUNT;
            if self.dma.r32(Self::tx_desc(i) + 8) as u64 & TX_DTYPE_MASK != TX_DTYPE_DONE { break; }
            self.tx_cons = self.tx_cons.wrapping_add(1);
        }
        dma_rmb();
    }

    /// Copy `f` into the next transmit buffer and hand it over by moving
    /// the tail. Ok(false) when every usable descriptor is still in flight.
    pub fn transmit<B: Bus>(&mut self, b: &mut B, f: &[u8]) -> R<bool, B::Error> {
        if !(MIN_FRAME..=MAX_FRAME).contains(&f.len()) { return Err(Error::Frame { len: f.len() }); }
        self.reclaim();
        if self.tx_pending() == TX_COUNT - 1 { return Ok(false); }
        let i = self.tx_prod % TX_COUNT;
        let buf = TX_BUFS + i * BUF_SIZE;
        let len = f.len().max(PAD_TO);
        self.dma.copy_in(buf, f);
        self.dma.zero(buf + f.len(), len - f.len());
        let d = Self::tx_desc(i);
        self.dma.w64(d, self.dma.at(buf));
        let cmd = TX_CMD_EOP | TX_CMD_RS | TX_CMD_ICRC;
        self.dma.w64(d + 8, cmd << TX_CMD_SHIFT | (len as u64) << TX_BUF_SZ_SHIFT);
        self.tx_prod = self.tx_prod.wrapping_add(1);
        dma_wmb();
        mmio_wmb();
        b.w32(qtx_tail(self.q()), (self.tx_prod % TX_COUNT) as u32)?;
        Ok(true)
    }

    /// The next good received frame: its length (CRC stripped) and
    /// destination, without taking it. Descriptors with errors, split
    /// frames and runts are given back on the way.
    pub fn peek<B: Bus>(&mut self, b: &mut B) -> R<Option<(usize, [u8; 6])>, B::Error> {
        if !self.running { return Ok(None); }
        loop {
            let d = Self::rx_desc(self.rx_next);
            if self.dma.r32(d + 8) as u64 & RX_DD == 0 { return Ok(None); }
            dma_rmb();
            let qw1 = self.dma.r64(d + 8);
            let len = ((qw1 >> RX_LEN_SHIFT) & RX_LEN_MASK) as usize;
            if qw1 & (RX_ERR_RXE | RX_ERR_OVERSIZE) != 0 || qw1 & RX_EOF == 0
                || !(MIN_FRAME..=BUF_SIZE).contains(&len) {
                self.rx_errors += 1;
                self.skip(b)?;
                continue;
            }
            let mut dst = [0u8; 6];
            self.dma.copy_out(RX_BUFS + self.rx_next * BUF_SIZE, &mut dst);
            return Ok(Some((len, dst)));
        }
    }

    /// Give the current receive descriptor back without reading it. The
    /// tail moves once a batch of 8 is ready.
    pub fn skip<B: Bus>(&mut self, b: &mut B) -> R<(), B::Error> {
        let i = self.rx_next;
        self.post_rx(i);
        self.rx_next = (i + 1) % RX_COUNT;
        if i.is_multiple_of(RX_BATCH) {
            dma_wmb();
            mmio_wmb();
            b.w32(qrx_tail(self.q()), i as u32)?;
        }
        Ok(())
    }

    /// Take the next good frame into `out`: Ok(Some(len)), Ok(None) when
    /// nothing is waiting. A frame longer than `out` stays queued
    /// (`Error::Frame` with its length).
    pub fn receive<B: Bus>(&mut self, b: &mut B, out: &mut [u8]) -> R<Option<usize>, B::Error> {
        let Some((len, _)) = self.peek(b)? else { return Ok(None) };
        if len > out.len() { return Err(Error::Frame { len }); }
        self.dma.copy_out(RX_BUFS + self.rx_next * BUF_SIZE, &mut out[..len]);
        self.skip(b)?;
        Ok(Some(len))
    }
}

/// Request a queue on and wait (up to 100 ms) for its status to follow.
fn enable<B: Bus>(b: &mut B, reg: u32, tx: bool) -> R<(), B::Error> {
    let v = b.r32(reg)?;
    b.w32(reg, v | QENA_REQ)?;
    if !wait(b, 100, 1000, |b| Ok(rd(b, reg)? & QENA_STAT != 0))? {
        return Err(Error::QueueTimeout { tx, value: b.r32(reg)? });
    }
    Ok(())
}

/// Request a queue off and wait (up to 100 ms) for its status to clear.
fn disable<B: Bus>(b: &mut B, reg: u32, tx: bool) -> R<(), B::Error> {
    let v = rd(b, reg)?;
    b.w32(reg, v & !QENA_REQ)?;
    if !wait(b, 100, 1000, |b| Ok(rd(b, reg)? & QENA_STAT == 0))? {
        return Err(Error::QueueTimeout { tx, value: b.r32(reg)? });
    }
    Ok(())
}

/// The frame `check` sends: broadcast, ethertype 0x88b5 (IEEE local
/// experimental), padded to 60 bytes.
pub fn check_frame(mac: [u8; 6]) -> [u8; 60] {
    let mut f = [0u8; 60];
    f[0..6].copy_from_slice(&[0xff; 6]);
    f[6..12].copy_from_slice(&mac);
    f[12..14].copy_from_slice(&0x88b5u16.to_be_bytes());
    let text = b"stormnic-i40e DMA check";
    f[14..14 + text.len()].copy_from_slice(text);
    f
}

/// Send the check frame and wait up to 100 ms for the device to mark its
/// descriptor done: it fetched the descriptor and the buffer and wrote the
/// descriptor, so DMA works both ways. Ok(Some(ms)) when it did.
pub fn check<B: Bus>(b: &mut B, net: &mut Net) -> R<Option<usize>, B::Error> {
    if !net.transmit(b, &check_frame(net.mac))? { return Ok(None); }
    for ms in 0..=100 {
        net.reclaim();
        if net.tx_pending() == 0 { return Ok(Some(ms)); }
        b.delay_us(1000);
    }
    Ok(None)
}
