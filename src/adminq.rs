//! The admin queue: the PF's command channel to the device firmware.
//!
//! Two rings of 32-byte descriptors in the DMA region. The transmit queue
//! (ATQ) carries commands: software writes a descriptor at the tail and
//! moves PF_ATQT; the firmware executes it, writes the descriptor back
//! (flags DD and CMP, ERR with a return code in `retval`) and moves PF_ATQH
//! past it. The receive queue (ARQ) carries events (link changes): software
//! gives it buffers, the firmware fills one, writes the descriptor back and
//! moves PF_ARQH; software re-arms it and moves PF_ARQT.
//!
//! A descriptor: flags (0-1), opcode (2-3), data length (4-5), return value
//! (6-7), cookie (8-15), and 16 bytes of parameters (16-31). A command with a
//! buffer ("indirect") has BUF set and the buffer's device address in
//! parameter bytes 24-27 (high) and 28-31 (low); LB when the buffer is over
//! 512 bytes, RD when the firmware is to read it. Commands are issued one at
//! a time, so one buffer (`ATQ_BUF`) serves them all.

use super::barrier::{dma_rmb, dma_wmb, mmio_wmb};
use super::{
    rd, Bus, Dma, Error, Link, R, ARQ, ARQ_BUFS, ARQ_BUF_SIZE, ARQ_COUNT, ATQ, ATQ_BUF, ATQ_BUF_SIZE,
    ATQ_COUNT,
};

pub const PF_ATQBAL: u32 = 0x0008_0000;
pub const PF_ATQBAH: u32 = 0x0008_0100;
pub const PF_ATQLEN: u32 = 0x0008_0200;
pub const PF_ATQH: u32 = 0x0008_0300;
pub const PF_ATQT: u32 = 0x0008_0400;
pub const PF_ARQBAL: u32 = 0x0008_0080;
pub const PF_ARQBAH: u32 = 0x0008_0180;
pub const PF_ARQLEN: u32 = 0x0008_0280;
pub const PF_ARQH: u32 = 0x0008_0380;
pub const PF_ARQT: u32 = 0x0008_0480;
/// PF_A[TR]QLEN: the queue is enabled.
pub const LEN_ENABLE: u32 = 1 << 31;
pub const HEAD_MASK: u32 = 0x3ff;

// Descriptor flags.
pub const FLAG_DD: u16 = 0x0001;
pub const FLAG_CMP: u16 = 0x0002;
pub const FLAG_ERR: u16 = 0x0004;
pub const FLAG_LB: u16 = 0x0200;
pub const FLAG_RD: u16 = 0x0400;
pub const FLAG_BUF: u16 = 0x1000;
pub const FLAG_SI: u16 = 0x2000;
/// Buffers over this many bytes need FLAG_LB.
pub const LARGE_BUF: usize = 512;

// Opcodes.
pub const OP_GET_VERSION: u16 = 0x0001;
pub const OP_QUEUE_SHUTDOWN: u16 = 0x0003;
pub const OP_MAC_ADDRESS_READ: u16 = 0x0107;
pub const OP_CLEAR_PXE: u16 = 0x0110;
pub const OP_GET_SWITCH_CONFIG: u16 = 0x0200;
pub const OP_GET_VSI_PARAMS: u16 = 0x0212;
pub const OP_ADD_MACVLAN: u16 = 0x0250;
pub const OP_SET_VSI_PROMISC: u16 = 0x0254;
pub const OP_GET_PHY_ABILITIES: u16 = 0x0600;
pub const OP_SET_PHY_CONFIG: u16 = 0x0601;
pub const OP_RESTART_AN: u16 = 0x0605;
pub const OP_GET_LINK_STATUS: u16 = 0x0607;

// Return codes (`retval`) the driver treats specially.
pub const RC_EEXIST: u16 = 13;

/// Firmware command timeout: 250 ms, polled every 50 µs.
pub const TIMEOUT_POLLS: usize = 5000;
pub const POLL_US: usize = 50;

// Mac Address Read: response flags, buffer layout.
pub const MAC_LAN_VALID: u16 = 0x10;
pub const MAC_READ_LEN: usize = 24;
// Switch configuration: 16-byte header, then 16-byte elements.
pub const SWITCH_BUF: usize = 512;
pub const SW_ELEM_VSI: u8 = 19;
// VSI properties (128 bytes): queue mapping and the scheduler handle.
pub const VSI_PROPS_LEN: usize = 128;
pub const VSI_MAPPING_FLAGS: usize = 28;
pub const VSI_QUEUE_MAPPING: usize = 30;
pub const VSI_QS_HANDLE: usize = 96;
// Set VSI Promiscuous Modes flags.
pub const PROMISC_UNICAST: u16 = 0x01;
pub const PROMISC_MULTICAST: u16 = 0x02;
pub const PROMISC_BROADCAST: u16 = 0x04;
pub const PROMISC_RX_ONLY: u16 = 0x8000;
// Add MAC/VLAN element flags.
pub const MACVLAN_PERFECT: u16 = 0x0001;
pub const MACVLAN_IGNORE_VLAN: u16 = 0x0004;
pub const MACVLAN_ELEM_LEN: usize = 16;
// Get Link Status: event enable, response bits.
pub const LSE_ENABLE: u16 = 0x3;
pub const LINK_UP: u8 = 0x01;
/// Link info: a module or cable is there.
pub const MEDIA_AVAILABLE: u8 = 0x40;
/// Link config byte (parameter 10): the FEC the link runs.
pub const LINK_FEC_KR: u8 = 0x01;
pub const LINK_FEC_RS: u8 = 0x02;
// Get PHY Abilities (answer of 0x218 bytes) / Set PHY Config.
pub const PHY_ABILITIES_LEN: usize = 0x218;
/// Abilities: pause TX/RX and low power, kept as they are.
pub const PHY_KEEP: u8 = 0x07;
pub const PHY_ENABLE_LINK: u8 = 0x08;
pub const PHY_ENABLE_AN: u8 = 0x10;
/// Set PHY Config only: restart the link with the new configuration.
pub const PHY_ATOMIC_LINK: u8 = 0x20;
/// Get PHY Abilities only: the PHY can run BASE-R / RS FEC.
pub const PHY_FEC_ABILITY_KR: u8 = 0x40;
pub const PHY_FEC_ABILITY_RS: u8 = 0x80;
/// The FEC configuration byte (answer byte 14 / Set PHY Config byte 14).
pub const FEC_ABILITY_KR: u8 = 0x01;
pub const FEC_ABILITY_RS: u8 = 0x02;
pub const FEC_REQUEST_KR: u8 = 0x04;
pub const FEC_REQUEST_RS: u8 = 0x08;
pub const FEC_AUTO: u8 = 0x10;
pub const FEC_MASK: u8 = 0x1f;
// Restart AN.
pub const AN_RESTART: u8 = 0x02;
pub const AN_LINK_ENABLE: u8 = 0x04;
// Clear PXE.
pub const CLEAR_PXE_RX_CNT: u8 = 0x2;

/// One admin-queue descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Desc(pub [u8; 32]);

impl Desc {
    /// A direct command: SI set, as the firmware expects of software.
    pub fn new(opcode: u16) -> Desc {
        let mut d = Desc([0; 32]);
        d.set16(0, FLAG_SI);
        d.set16(2, opcode);
        d
    }
    pub fn get16(&self, off: usize) -> u16 { u16::from_le_bytes([self.0[off], self.0[off + 1]]) }
    pub fn set16(&mut self, off: usize, v: u16) { self.0[off..off + 2].copy_from_slice(&v.to_le_bytes()); }
    pub fn get32(&self, off: usize) -> u32 {
        u32::from_le_bytes([self.0[off], self.0[off + 1], self.0[off + 2], self.0[off + 3]])
    }
    pub fn set32(&mut self, off: usize, v: u32) { self.0[off..off + 4].copy_from_slice(&v.to_le_bytes()); }
    pub fn flags(&self) -> u16 { self.get16(0) }
    pub fn opcode(&self) -> u16 { self.get16(2) }
    pub fn datalen(&self) -> u16 { self.get16(4) }
    pub fn retval(&self) -> u16 { self.get16(6) }
    /// Parameter byte `i` (0..16), descriptor byte 16 + i.
    pub fn param(&self, i: usize) -> u8 { self.0[16 + i] }
    pub fn param16(&self, i: usize) -> u16 { self.get16(16 + i) }
    pub fn param32(&self, i: usize) -> u32 { self.get32(16 + i) }
    pub fn set_param(&mut self, i: usize, v: u8) { self.0[16 + i] = v; }
    pub fn set_param16(&mut self, i: usize, v: u16) { self.set16(16 + i, v); }
    pub fn set_param32(&mut self, i: usize, v: u32) { self.set32(16 + i, v); }
}

/// The firmware's versions (Get Version).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Version {
    pub rom: u32,
    pub fw_build: u32,
    pub fw_major: u16,
    pub fw_minor: u16,
    pub api_major: u16,
    pub api_minor: u16,
}

/// The PHY's current configuration (Get PHY Abilities, first 15 bytes of
/// the answer), which Set PHY Config takes back with the FEC changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct PhyConfig {
    /// Bitmap of PHY types (bit n: PHY type n) and of the 25G/2.5G/5G ones.
    pub phy_types: u32,
    pub phy_types_ext: u8,
    /// Get Link Status speed codes, one bit each.
    pub speeds: u8,
    pub abilities: u8,
    pub eee: u16,
    pub eeer: u32,
    pub low_power: u8,
    /// The FEC configuration byte (`FEC_*`), masked.
    pub fec: u8,
}

/// The VSI the firmware made for this PF.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Vsi {
    pub seid: u16,
    /// Its first queue (PF-relative): the one queue pair this driver uses.
    pub queue: u16,
    /// The transmit scheduler's queue-set handle for traffic class 0.
    pub qset: u16,
}

/// The two admin rings in the DMA region and where software is in them.
pub struct AdminQ {
    pub dma: Dma,
    /// Next ATQ descriptor to use.
    ntu: usize,
    /// Next ARQ descriptor to look at.
    ntc: usize,
    pub api: (u16, u16),
}

impl AdminQ {
    pub fn new(dma: Dma) -> AdminQ { AdminQ { dma, ntu: 0, ntc: 0, api: (0, 0) } }

    fn arm_event(&self, i: usize) {
        let d = ARQ + i * 32;
        self.dma.zero(d, 32);
        let mut flags = FLAG_BUF;
        if ARQ_BUF_SIZE > LARGE_BUF { flags |= FLAG_LB; }
        self.dma.w16(d, flags);
        self.dma.w16(d + 4, ARQ_BUF_SIZE as u16);
        let buf = self.dma.at(ARQ_BUFS + i * ARQ_BUF_SIZE);
        self.dma.w32(d + 24, (buf >> 32) as u32);
        self.dma.w32(d + 28, buf as u32);
    }

    /// Turn both queues off and clear their registers: the firmware stops
    /// using the rings. Done before (re)programming them, since a previous
    /// driver's queues may still point into its memory, and before the
    /// region is freed.
    pub fn disable<B: Bus>(&mut self, b: &mut B) -> R<(), B::Error> {
        for (len, h, t, bal, bah) in [
            (PF_ATQLEN, PF_ATQH, PF_ATQT, PF_ATQBAL, PF_ATQBAH),
            (PF_ARQLEN, PF_ARQH, PF_ARQT, PF_ARQBAL, PF_ARQBAH),
        ] {
            b.w32(len, 0)?;
            b.w32(h, 0)?;
            b.w32(t, 0)?;
            b.w32(bal, 0)?;
            b.w32(bah, 0)?;
        }
        Ok(())
    }

    /// Program both queues (disabled first). The ARQ gets all its buffers.
    pub fn init<B: Bus>(&mut self, b: &mut B) -> R<(), B::Error> {
        self.disable(b)?;
        self.dma.zero(ATQ, ATQ_COUNT * 32);
        for i in 0..ARQ_COUNT { self.arm_event(i); }
        self.ntu = 0;
        self.ntc = 0;
        dma_wmb();
        mmio_wmb();
        let atq = self.dma.at(ATQ);
        b.w32(PF_ATQH, 0)?;
        b.w32(PF_ATQT, 0)?;
        b.w32(PF_ATQLEN, ATQ_COUNT as u32 | LEN_ENABLE)?;
        b.w32(PF_ATQBAL, atq as u32)?;
        b.w32(PF_ATQBAH, (atq >> 32) as u32)?;
        let bal = b.r32(PF_ATQBAL)?;
        if bal != atq as u32 { return Err(Error::AdminSetup { bal }); }
        let arq = self.dma.at(ARQ);
        b.w32(PF_ARQH, 0)?;
        b.w32(PF_ARQT, 0)?;
        b.w32(PF_ARQLEN, ARQ_COUNT as u32 | LEN_ENABLE)?;
        b.w32(PF_ARQBAL, arq as u32)?;
        b.w32(PF_ARQBAH, (arq >> 32) as u32)?;
        let bal = b.r32(PF_ARQBAL)?;
        if bal != arq as u32 { return Err(Error::AdminSetup { bal }); }
        b.w32(PF_ARQT, (ARQ_COUNT - 1) as u32)?;
        Ok(())
    }

    /// Issue `desc` and wait for the firmware's answer, which replaces it.
    /// `send` goes into the buffer first (FLAG_RD); with `buf_len` > 0 the
    /// command carries the buffer, whose contents the caller reads from
    /// `ATQ_BUF` afterwards (`response`).
    pub fn command<B: Bus>(&mut self, b: &mut B, desc: &mut Desc, send: &[u8], buf_len: usize)
        -> R<(), B::Error> {
        let opcode = desc.opcode();
        let len = buf_len.max(send.len()).min(ATQ_BUF_SIZE);
        if len > 0 {
            self.dma.zero(ATQ_BUF, len);
            self.dma.copy_in(ATQ_BUF, &send[..send.len().min(len)]);
            let mut flags = desc.flags() | FLAG_BUF;
            if len > LARGE_BUF { flags |= FLAG_LB; }
            if !send.is_empty() { flags |= FLAG_RD; }
            desc.set16(0, flags);
            desc.set16(4, len as u16);
            let buf = self.dma.at(ATQ_BUF);
            desc.set32(24, (buf >> 32) as u32);
            desc.set32(28, buf as u32);
        }
        let slot = ATQ + self.ntu * 32;
        self.dma.copy_in(slot, &desc.0);
        self.ntu = (self.ntu + 1) % ATQ_COUNT;
        dma_wmb();
        mmio_wmb();
        b.w32(PF_ATQT, self.ntu as u32)?;
        let mut head = 0;
        let mut done = false;
        for _ in 0..=TIMEOUT_POLLS {
            head = rd(b, PF_ATQH)? & HEAD_MASK;
            if head as usize == self.ntu { done = true; break; }
            b.delay_us(POLL_US);
        }
        dma_rmb();
        self.dma.copy_out(slot, &mut desc.0);
        if !done || desc.flags() & FLAG_DD == 0 {
            return Err(Error::AdminTimeout { opcode, head });
        }
        if desc.flags() & FLAG_ERR != 0 || desc.retval() != 0 {
            return Err(Error::Admin { opcode, retval: desc.retval() });
        }
        Ok(())
    }

    /// Bytes of the last command's buffer.
    pub fn response(&self, off: usize, out: &mut [u8]) { self.dma.copy_out(super::ATQ_BUF + off, out); }

    /// Get Version; refuses firmware whose API major is not 1.
    pub fn version<B: Bus>(&mut self, b: &mut B) -> R<Version, B::Error> {
        let mut d = Desc::new(OP_GET_VERSION);
        self.command(b, &mut d, &[], 0)?;
        let v = Version {
            rom: d.param32(0),
            fw_build: d.param32(4),
            fw_major: d.param16(8),
            fw_minor: d.param16(10),
            api_major: d.param16(12),
            api_minor: d.param16(14),
        };
        self.api = (v.api_major, v.api_minor);
        if v.api_major != 1 { return Err(Error::AdminApi { major: v.api_major, minor: v.api_minor }); }
        Ok(v)
    }

    /// Clear PXE mode (the firmware's boot-time queue handling). Ok(false)
    /// when it was already clear (EEXIST).
    pub fn clear_pxe<B: Bus>(&mut self, b: &mut B) -> R<bool, B::Error> {
        let mut d = Desc::new(OP_CLEAR_PXE);
        d.set_param(0, CLEAR_PXE_RX_CNT);
        let r = match self.command(b, &mut d, &[], 0) {
            Ok(()) => Ok(true),
            Err(Error::Admin { retval: RC_EEXIST, .. }) => Ok(false),
            Err(e) => Err(e),
        };
        // Then the PXE-mode bit itself, as the shared code does.
        b.w32(super::GLLAN_RCTL_0, 1)?;
        r
    }

    /// Manage MAC Address Read: the PF's LAN address.
    pub fn mac_read<B: Bus>(&mut self, b: &mut B) -> R<[u8; 6], B::Error> {
        let mut d = Desc::new(OP_MAC_ADDRESS_READ);
        self.command(b, &mut d, &[], MAC_READ_LEN)?;
        let mut mac = [0u8; 6];
        self.response(0, &mut mac);
        if d.param16(0) & MAC_LAN_VALID == 0 || !super::unicast(mac) { return Err(Error::NoMac); }
        Ok(mac)
    }

    /// Get Switch Configuration, page by page: the first VSI element's SEID.
    pub fn find_vsi<B: Bus>(&mut self, b: &mut B) -> R<u16, B::Error> {
        let mut next = 0u16;
        // A switch has a handful of elements; the bound only stops a
        // firmware that keeps answering with a next SEID.
        for _ in 0..64 {
            let mut d = Desc::new(OP_GET_SWITCH_CONFIG);
            d.set_param16(0, next);
            self.command(b, &mut d, &[], SWITCH_BUF)?;
            let mut header = [0u8; 2];
            self.response(0, &mut header);
            let reported = (u16::from_le_bytes(header) as usize).min(SWITCH_BUF / 16 - 1);
            for i in 0..reported {
                let mut e = [0u8; 16];
                self.response(16 + i * 16, &mut e);
                if e[0] == SW_ELEM_VSI { return Ok(u16::from_le_bytes([e[2], e[3]])); }
            }
            next = d.param16(0);
            if next == 0 { break; }
        }
        Err(Error::NoVsi)
    }

    /// Get VSI Parameters: its first queue and queue-set handle.
    pub fn vsi<B: Bus>(&mut self, b: &mut B, seid: u16) -> R<Vsi, B::Error> {
        let mut d = Desc::new(OP_GET_VSI_PARAMS);
        d.set_param16(0, seid);
        self.command(b, &mut d, &[], VSI_PROPS_LEN)?;
        let mut q = [0u8; 2];
        self.response(VSI_QUEUE_MAPPING, &mut q);
        let mut h = [0u8; 2];
        self.response(VSI_QS_HANDLE, &mut h);
        Ok(Vsi { seid, queue: u16::from_le_bytes(q) & 0x7ff, qset: u16::from_le_bytes(h) })
    }

    /// Add a perfect MAC filter for `mac` on the VSI, any VLAN. Ok(false)
    /// when it existed already.
    pub fn add_mac<B: Bus>(&mut self, b: &mut B, seid: u16, mac: [u8; 6]) -> R<bool, B::Error> {
        let mut d = Desc::new(OP_ADD_MACVLAN);
        d.set_param16(0, 1);
        d.set_param16(2, seid | 0x8000);
        let mut e = [0u8; MACVLAN_ELEM_LEN];
        e[..6].copy_from_slice(&mac);
        e[8..10].copy_from_slice(&(MACVLAN_PERFECT | MACVLAN_IGNORE_VLAN).to_le_bytes());
        match self.command(b, &mut d, &e, MACVLAN_ELEM_LEN) {
            Ok(()) => Ok(true),
            Err(Error::Admin { retval: RC_EEXIST, .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Set VSI Promiscuous Modes: unicast, multicast and broadcast at once.
    /// Unicast promiscuity is receive-only on API 1.5 and later.
    pub fn promiscuous<B: Bus>(&mut self, b: &mut B, seid: u16, unicast: bool, multicast: bool,
        broadcast: bool) -> R<(), B::Error> {
        let mut d = Desc::new(OP_SET_VSI_PROMISC);
        let mut flags = 0;
        if unicast { flags |= PROMISC_UNICAST; }
        if multicast { flags |= PROMISC_MULTICAST; }
        if broadcast { flags |= PROMISC_BROADCAST; }
        let mut valid = PROMISC_UNICAST | PROMISC_MULTICAST | PROMISC_BROADCAST;
        if self.api >= (1, 5) {
            valid |= PROMISC_RX_ONLY;
            if unicast { flags |= PROMISC_RX_ONLY; }
        }
        d.set_param16(0, flags);
        d.set_param16(2, valid);
        d.set_param16(4, seid & 0x3ff);
        self.command(b, &mut d, &[], 0)
    }

    /// Get Link Status; `events` asks the firmware to report changes on the
    /// ARQ from now on.
    pub fn link<B: Bus>(&mut self, b: &mut B, events: bool) -> R<Link, B::Error> {
        let mut d = Desc::new(OP_GET_LINK_STATUS);
        if events { d.set_param16(0, LSE_ENABLE); }
        self.command(b, &mut d, &[], 0)?;
        Ok(link_of(&d))
    }

    /// Restart autonegotiation with the link enabled (a previous driver may
    /// have left it disabled).
    pub fn restart_an<B: Bus>(&mut self, b: &mut B) -> R<(), B::Error> {
        let mut d = Desc::new(OP_RESTART_AN);
        d.set_param(0, AN_RESTART | AN_LINK_ENABLE);
        self.command(b, &mut d, &[], 0)
    }

    /// Get PHY Abilities, the current configuration (parameter 0 = 0: not
    /// the qualified-module list, not the NVM's initial values).
    pub fn phy_config<B: Bus>(&mut self, b: &mut B) -> R<PhyConfig, B::Error> {
        let mut d = Desc::new(OP_GET_PHY_ABILITIES);
        self.command(b, &mut d, &[], PHY_ABILITIES_LEN)?;
        let mut a = [0u8; 16];
        self.response(0, &mut a);
        Ok(PhyConfig {
            phy_types: u32::from_le_bytes([a[0], a[1], a[2], a[3]]),
            speeds: a[4],
            abilities: a[5],
            eee: u16::from_le_bytes([a[6], a[7]]),
            eeer: u32::from_le_bytes([a[8], a[9], a[10], a[11]]),
            low_power: a[12],
            phy_types_ext: a[13],
            fec: a[14] & FEC_MASK,
        })
    }

    /// Set PHY Config: `c` with the link enabled and restarted at once
    /// (atomic) on the new configuration. Autonegotiation and the pause and
    /// low-power bits stay as `c` has them.
    pub fn set_phy_config<B: Bus>(&mut self, b: &mut B, c: &PhyConfig) -> R<(), B::Error> {
        let mut d = Desc::new(OP_SET_PHY_CONFIG);
        d.set_param32(0, c.phy_types);
        d.set_param(4, c.speeds);
        d.set_param(5, (c.abilities & (PHY_KEEP | PHY_ENABLE_AN)) | PHY_ENABLE_LINK | PHY_ATOMIC_LINK);
        d.set_param16(6, c.eee);
        d.set_param32(8, c.eeer);
        d.set_param(12, c.low_power);
        d.set_param(13, c.phy_types_ext);
        d.set_param(14, c.fec & FEC_MASK);
        self.command(b, &mut d, &[], 0)
    }

    /// Queue Shutdown: tell the firmware the driver is unloading.
    pub fn shutdown<B: Bus>(&mut self, b: &mut B) -> R<(), B::Error> {
        let mut d = Desc::new(OP_QUEUE_SHUTDOWN);
        d.set_param32(0, 1);
        self.command(b, &mut d, &[], 0)
    }

    /// Take every event the firmware has written, re-arming each; the last
    /// link state among them, if any.
    pub fn events<B: Bus>(&mut self, b: &mut B) -> R<Option<Link>, B::Error> {
        let mut link = None;
        let head = (rd(b, PF_ARQH)? & HEAD_MASK) as usize;
        let mut moved = false;
        while self.ntc != head && head < ARQ_COUNT {
            dma_rmb();
            let mut d = Desc([0; 32]);
            self.dma.copy_out(ARQ + self.ntc * 32, &mut d.0);
            if d.opcode() == OP_GET_LINK_STATUS { link = Some(link_of(&d)); }
            self.arm_event(self.ntc);
            self.ntc = (self.ntc + 1) % ARQ_COUNT;
            moved = true;
        }
        if moved {
            dma_wmb();
            mmio_wmb();
            // The tail stays one behind the next descriptor software reads.
            b.w32(PF_ARQT, ((self.ntc + ARQ_COUNT - 1) % ARQ_COUNT) as u32)?;
        }
        Ok(link)
    }
}

/// A Get Link Status answer (command write-back or event).
pub fn link_of(d: &Desc) -> Link {
    let info = d.param(4);
    let up = info & LINK_UP != 0;
    Link {
        up,
        mbps: if up { super::speed_mbps(d.param(3)) } else { 0 },
        phy_type: d.param(2),
        info,
        an: d.param(5),
        config: d.param(10),
    }
}
