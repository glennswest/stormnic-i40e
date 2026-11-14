//! A simulated 700-series PF, shared by `test/i40e.rs` and `test/snp.rs`.
//!
//! It models what the driver relies on, from the same facts as the driver
//! (docs/hardware.md) but written independently of it:
//!
//! - registers in a map, with the reset handshakes (GLGEN_RSTAT, GLNVM_ULD,
//!   PFGEN_CTRL.PFSWR) and the queue enable handshakes (QENA_REQ/STAT);
//! - a firmware that executes admin commands from the ATQ in host memory
//!   when ATQT moves (checking flags, lengths and buffer addresses), writes
//!   each descriptor back and moves ATQH, and posts link events to the ARQ;
//! - the port: a media (PHY type), the PHY configuration Get PHY Abilities
//!   reports and Set PHY Config replaces, the FEC the link runs from it, and
//!   a link partner that links only on the FEC it runs (25G);
//! - the HMC: segment descriptors written through PFHMC_SDCMD, the context
//!   of a queue found through the paged SD and its PD page when the queue is
//!   enabled, decoded field by field and checked;
//! - the queues: transmit descriptors fetched on a tail write and marked
//!   done, received frames written into the receive ring through the
//!   switch's filters, and descriptor write-back held back until four are
//!   pending unless write-back-on-ITR is armed for the queue.
#![allow(dead_code)]

use crate::i40e::adminq::*;
use crate::i40e::hmc::{self, get};
use crate::i40e::*;
use std::collections::{BTreeMap, VecDeque};

/// Above 4 GB, so the high halves of the base registers matter.
pub const DEVICE: u64 = 0x1_2345_0000;
pub const MAC: [u8; 6] = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];
pub const PF: u32 = 1;
pub const FIRSTQ: u32 = 64;
pub const LASTQ: u32 = 127;
pub const VSI_SEID: u16 = 0x0390;
pub const QSET: u16 = 0x0123;
/// PHY types from here to 0x10 are not media (unrecognized, unsupported).
const PHY_UNRECOGNIZED_SIM: u8 = 0x0e;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ctx {
    pub base: u64,
    pub qlen: u64,
}

pub struct Nic {
    pub regs: BTreeMap<u32, u32>,
    /// Every register write, in order.
    pub ops: Vec<(u32, u32)>,
    pub mem: *mut u8,
    pub removed: bool,
    // Reset.
    /// GLGEN_RSTAT reads "in reset" this many more times.
    pub global_reset_polls: usize,
    /// PFGEN_CTRL.PFSWR stays set this many reads (usize::MAX: forever).
    pub pfswr_polls: usize,
    pub pf_resets: usize,
    // Firmware.
    pub api: (u16, u16),
    pub commands: Vec<u16>,
    /// The command (opcode) the firmware answers with this error code.
    pub refuse: Option<(u16, u16)>,
    /// The firmware stops answering at this opcode.
    pub hang: Option<u16>,
    pub pxe: bool,
    pub filters: Vec<[u8; 6]>,
    pub promisc: (bool, bool, bool),
    pub promisc_flags: Vec<(u16, u16)>,
    pub lse: bool,
    pub link_up: bool,
    /// After Restart AN, the link comes up at this many delays (None: never).
    pub an_link_after: Option<usize>,
    pub an_restarts: usize,
    pub an_timer: Option<usize>,
    /// The media's Get Link Status PHY type (0xfe: none).
    pub phy: u8,
    /// A module or cable is in (link info MEDIA_AVAILABLE).
    pub media: bool,
    /// The PHY configuration (Get PHY Abilities), FEC byte included.
    pub phy_cfg: PhyConfig,
    /// Every Set PHY Config, as its 16 parameter bytes.
    pub set_phy: Vec<[u8; 16]>,
    /// The FEC the link partner runs (Get Link Status config bits) on a
    /// 25G media; after a link restart the link comes up only on it.
    pub partner_fec: u8,
    pub shutdown: bool,
    /// Elements of the switch configuration, one page each.
    pub switch_pages: Vec<Vec<(u8, u16)>>,
    pub vsi_queue: u16,
    // Queues.
    pub rx_ctx: Option<Ctx>,
    pub tx_ctx: Option<Ctx>,
    pub rx_head: usize,
    pub tx_head: usize,
    /// Register whose QENA_STAT never follows the request.
    pub stuck: Option<u32>,
    pub loopback: bool,
    pub wire: Vec<Vec<u8>>,
    pub inbound: Vec<Vec<u8>>,
    pub dropped: usize,
    /// Receive descriptors written, waiting for write-back.
    pub rx_wb: VecDeque<(u64, u64)>,
    pub tx_wb: VecDeque<u64>,
    pub delays: usize,
    pub rx_tails: Vec<u32>,
    /// HMC segment descriptors written through PFHMC_SDCMD: (low, high).
    pub sd: BTreeMap<u32, (u32, u32)>,
}

impl Nic {
    pub fn new(mem: &mut [u8]) -> Self {
        let mut regs = BTreeMap::new();
        regs.insert(GLGEN_RSTCTL, 8);
        regs.insert(GLNVM_ULD, ULD_CORE_DONE | ULD_GLOBAL_DONE);
        regs.insert(PF_FUNC_RID, 0x8100 | PF);
        regs.insert(PFGEN_PORTNUM, 1);
        regs.insert(PFLAN_QALLOC, QALLOC_VALID | LASTQ << 16 | FIRSTQ);
        regs.insert(hmc::GLHMC_LANQMAX, 1536);
        regs.insert(hmc::GLHMC_LANTXOBJSZ, 7);
        regs.insert(hmc::GLHMC_LANRXOBJSZ, 5);
        Nic {
            regs, ops: vec![], mem: mem.as_mut_ptr(), removed: false,
            global_reset_polls: 0, pfswr_polls: 2, pf_resets: 0,
            api: (1, 15), commands: vec![], refuse: None, hang: None, pxe: true,
            filters: vec![MAC], promisc: (false, false, true), promisc_flags: vec![], lse: false,
            link_up: true, an_link_after: Some(3), an_restarts: 0, an_timer: None, shutdown: false,
            phy: 0x14, media: true, phy_cfg: Self::phy_config_for(0x14), set_phy: vec![], partner_fec: 0,
            switch_pages: vec![vec![(1, 0x0002), (17, 0x0200)], vec![(SW_ELEM_VSI, VSI_SEID)]],
            vsi_queue: 0,
            rx_ctx: None, tx_ctx: None, rx_head: 0, tx_head: 0, stuck: None, loopback: false,
            wire: vec![], inbound: vec![], dropped: 0, rx_wb: VecDeque::new(), tx_wb: VecDeque::new(),
            delays: 0, rx_tails: vec![], sd: BTreeMap::new(),
        }
    }

    /// A firmware's PHY configuration for a port with media `phy`: that
    /// PHY type enabled with autonegotiation and the link, and for a 25G
    /// media the XXV710's default, FEC auto with both abilities.
    pub fn phy_config_for(phy: u8) -> PhyConfig {
        let is25 = (0x1f..=0x24).contains(&phy);
        PhyConfig {
            phy_types: if is25 { 0 } else { 1 << phy },
            phy_types_ext: if is25 { 1 << (phy - 0x1f) } else { 0 },
            speeds: if is25 { 0x40 } else { 0x08 },
            abilities: PHY_ENABLE_LINK | PHY_ENABLE_AN | if is25 { PHY_FEC_ABILITY_KR | PHY_FEC_ABILITY_RS } else { 0 },
            eee: 0,
            eeer: 0x1234_5678,
            low_power: 0,
            fec: if is25 { FEC_AUTO | FEC_ABILITY_KR | FEC_ABILITY_RS } else { 0 },
        }
    }

    /// Put media `phy` in, with the firmware's configuration for it and a
    /// partner running `partner_fec`.
    pub fn with_media(&mut self, phy: u8, partner_fec: u8) {
        self.phy = phy;
        self.phy_cfg = Self::phy_config_for(phy);
        self.partner_fec = partner_fec;
    }

    /// The Get Link Status config bits the link runs on the configuration:
    /// none below 25G; on 25G, what the FEC byte asks for (auto: what the
    /// partner runs).
    pub fn running_fec(&self) -> u8 {
        if !(0x1f..=0x24).contains(&self.phy) { return 0; }
        let c = self.phy_cfg.fec;
        if c & FEC_AUTO != 0 { return self.partner_fec; }
        if c & (FEC_REQUEST_RS | FEC_ABILITY_RS) != 0 { LINK_FEC_RS }
        else if c & (FEC_REQUEST_KR | FEC_ABILITY_KR) != 0 { LINK_FEC_KR }
        else { 0 }
    }

    /// The link can come up: media in, and on 25G the partner's FEC.
    pub fn can_link(&self) -> bool {
        let runnable = self.phy < PHY_UNRECOGNIZED_SIM || (0x11..=0x31).contains(&self.phy);
        let is25 = (0x1f..=0x24).contains(&self.phy);
        self.media && runnable && (!is25 || self.running_fec() == self.partner_fec)
    }

    pub fn reg(&self, r: u32) -> u32 { *self.regs.get(&r).unwrap_or(&0) }
    pub fn set(&mut self, r: u32, v: u32) { self.regs.insert(r, v); }
    pub fn wrote(&self, r: u32) -> Vec<u32> { self.ops.iter().filter(|o| o.0 == r).map(|o| o.1).collect() }
    pub fn pos(&self, op: (u32, u32)) -> usize {
        self.ops.iter().position(|o| *o == op).unwrap_or_else(|| panic!("no write {:#x} = {:#x}", op.0, op.1))
    }

    pub fn last(&self, op: (u32, u32)) -> usize {
        self.ops.iter().rposition(|o| *o == op).unwrap_or_else(|| panic!("no write {:#x} = {:#x}", op.0, op.1))
    }
    /// The first write to `r`, whatever its value.
    pub fn first(&self, r: u32) -> usize {
        self.ops.iter().position(|o| o.0 == r).unwrap_or_else(|| panic!("no write to {r:#x}"))
    }

    /// Host pointer for a device address in the DMA region.
    pub fn at(&self, device: u64, len: usize) -> *mut u8 {
        assert!(device >= DEVICE && device + len as u64 <= DEVICE + DMA_BYTES as u64,
            "DMA outside the region: {device:#x} + {len}");
        unsafe { self.mem.add((device - DEVICE) as usize) }
    }
    pub fn read(&self, device: u64, out: &mut [u8]) {
        let p = self.at(device, out.len());
        for (k, v) in out.iter_mut().enumerate() { *v = unsafe { *p.add(k) }; }
    }
    pub fn write(&self, device: u64, data: &[u8]) {
        let p = self.at(device, data.len());
        for (k, v) in data.iter().enumerate() { unsafe { *p.add(k) = *v; } }
    }
    pub fn r64(&self, device: u64) -> u64 {
        let mut b = [0u8; 8];
        self.read(device, &mut b);
        u64::from_le_bytes(b)
    }
    pub fn w64(&self, device: u64, v: u64) { self.write(device, &v.to_le_bytes()); }

    // ---- Firmware ----

    fn base(&self, lo: u32, hi: u32) -> u64 { self.reg(lo) as u64 | (self.reg(hi) as u64) << 32 }

    fn run_atq(&mut self) {
        if self.reg(PF_ATQLEN) & LEN_ENABLE == 0 { return; }
        let count = (self.reg(PF_ATQLEN) & HEAD_MASK) as usize;
        let ring = self.base(PF_ATQBAL, PF_ATQBAH);
        while self.reg(PF_ATQH) != self.reg(PF_ATQT) {
            let h = self.reg(PF_ATQH) as usize;
            let mut d = Desc([0; 32]);
            self.read(ring + 32 * h as u64, &mut d.0);
            if self.hang == Some(d.opcode()) { return; }
            self.execute(&mut d);
            self.write(ring + 32 * h as u64, &d.0);
            self.set(PF_ATQH, ((h + 1) % count) as u32);
        }
    }

    fn buffer(&self, d: &Desc) -> (u64, usize) {
        assert!(d.flags() & FLAG_BUF != 0, "indirect command {:#x} without BUF", d.opcode());
        let len = d.datalen() as usize;
        assert_eq!(d.flags() & FLAG_LB != 0, len > LARGE_BUF, "LB flag for a {len}-byte buffer");
        ((d.param32(8) as u64) << 32 | d.param32(12) as u64, len)
    }

    fn execute(&mut self, d: &mut Desc) {
        let op = d.opcode();
        self.commands.push(op);
        assert!(d.flags() & FLAG_SI != 0, "command {op:#x} without SI");
        let mut retval = 0u16;
        match op {
            OP_GET_VERSION => {
                d.set_param32(0, 0x1234);
                d.set_param32(4, 0xabcd);
                d.set_param16(8, 9);
                d.set_param16(10, 1);
                d.set_param16(12, self.api.0);
                d.set_param16(14, self.api.1);
            }
            OP_CLEAR_PXE => {
                assert_eq!(d.param(0), 2, "Clear PXE rx_cnt");
                if self.pxe { self.pxe = false } else { retval = RC_EEXIST }
            }
            OP_MAC_ADDRESS_READ => {
                let (buf, len) = self.buffer(d);
                assert!(len >= 24);
                self.write(buf, &MAC);
                d.set_param16(0, MAC_LAN_VALID | 0x40);
            }
            OP_GET_SWITCH_CONFIG => {
                let (buf, len) = self.buffer(d);
                let page = d.param16(0) as usize;
                let elems = self.switch_pages.get(page).cloned().unwrap_or_default();
                assert!(16 + 16 * elems.len() <= len);
                let mut b = vec![0u8; 16 + 16 * elems.len()];
                b[0..2].copy_from_slice(&(elems.len() as u16).to_le_bytes());
                for (i, (kind, seid)) in elems.iter().enumerate() {
                    b[16 + 16 * i] = *kind;
                    b[18 + 16 * i..20 + 16 * i].copy_from_slice(&seid.to_le_bytes());
                }
                self.write(buf, &b);
                let next = if page + 1 < self.switch_pages.len() { page + 1 } else { 0 };
                d.set_param16(0, next as u16);
            }
            OP_GET_VSI_PARAMS => {
                let (buf, len) = self.buffer(d);
                assert_eq!(len, 128);
                if d.param16(0) != VSI_SEID { retval = 2; } else {
                    let mut b = [0u8; 128];
                    b[30..32].copy_from_slice(&self.vsi_queue.to_le_bytes());
                    b[96..98].copy_from_slice(&QSET.to_le_bytes());
                    b[98..100].copy_from_slice(&0xffffu16.to_le_bytes());
                    self.write(buf, &b);
                }
            }
            OP_ADD_MACVLAN => {
                let (buf, len) = self.buffer(d);
                assert!(d.flags() & FLAG_RD != 0, "Add MAC/VLAN buffer is read by the firmware");
                assert_eq!(d.param16(0), 1);
                assert_eq!(d.param16(2), VSI_SEID | 0x8000);
                let mut e = vec![0u8; len];
                self.read(buf, &mut e);
                let mut m = [0u8; 6];
                m.copy_from_slice(&e[..6]);
                assert_eq!(u16::from_le_bytes([e[8], e[9]]), MACVLAN_PERFECT | MACVLAN_IGNORE_VLAN);
                if self.filters.iter().filter(|f| **f == m).count() > 1 { retval = RC_EEXIST } else { self.filters.push(m) }
            }
            OP_SET_VSI_PROMISC => {
                assert_eq!(d.param16(4), VSI_SEID & 0x3ff);
                let (flags, valid) = (d.param16(0), d.param16(2));
                self.promisc_flags.push((flags, valid));
                if valid & PROMISC_UNICAST != 0 { self.promisc.0 = flags & PROMISC_UNICAST != 0; }
                if valid & PROMISC_MULTICAST != 0 { self.promisc.1 = flags & PROMISC_MULTICAST != 0; }
                if valid & PROMISC_BROADCAST != 0 { self.promisc.2 = flags & PROMISC_BROADCAST != 0; }
            }
            OP_GET_LINK_STATUS => {
                if d.param16(0) & 3 == LSE_ENABLE { self.lse = true; }
                self.link_params(d);
            }
            OP_RESTART_AN => {
                assert_eq!(d.param(0), AN_RESTART | AN_LINK_ENABLE);
                self.an_restarts += 1;
                self.link_up = false;
                self.an_timer = self.an_link_after;
            }
            OP_GET_PHY_ABILITIES => {
                let (buf, len) = self.buffer(d);
                assert_eq!(len, 0x218, "Get PHY Abilities answer length");
                assert_eq!(d.param16(0), 0, "current configuration, not qualified modules or initial values");
                let c = self.phy_cfg;
                let mut a = vec![0u8; len];
                a[0..4].copy_from_slice(&c.phy_types.to_le_bytes());
                a[4] = c.speeds;
                a[5] = c.abilities;
                a[6..8].copy_from_slice(&c.eee.to_le_bytes());
                a[8..12].copy_from_slice(&c.eeer.to_le_bytes());
                a[12] = c.low_power;
                a[13] = c.phy_types_ext;
                // The module-type bits above the FEC ones are not FEC.
                a[14] = c.fec | 0xa0;
                self.write(buf, &a);
            }
            OP_SET_PHY_CONFIG if self.refuse.is_some_and(|r| r.0 == op) => {} // refused: nothing changes
            OP_SET_PHY_CONFIG => {
                assert!(d.flags() & FLAG_BUF == 0, "Set PHY Config is direct");
                let mut p = [0u8; 16];
                p.copy_from_slice(&d.0[16..32]);
                self.set_phy.push(p);
                let a = d.param(5);
                assert!(a & PHY_ENABLE_LINK != 0, "Set PHY Config without the link enabled");
                assert!(a & PHY_ATOMIC_LINK != 0, "Set PHY Config without the atomic link restart");
                assert!(d.param32(0) != 0 || d.param(13) != 0, "Set PHY Config with no PHY type turns the link off");
                assert_eq!(d.param(14) & !FEC_MASK, 0, "FEC byte carries only FEC bits");
                self.phy_cfg = PhyConfig {
                    phy_types: d.param32(0), speeds: d.param(4), abilities: a & 0x1f, eee: d.param16(6),
                    eeer: d.param32(8), low_power: d.param(12), phy_types_ext: d.param(13), fec: d.param(14),
                };
                self.link_up = false;
                self.an_timer = self.an_link_after;
            }
            OP_QUEUE_SHUTDOWN => {
                assert_eq!(d.param32(0), 1);
                self.shutdown = true;
            }
            _ => retval = 3,
        }
        if let Some((o, rv)) = self.refuse {
            if o == op { retval = rv; }
        }
        let mut flags = d.flags() | FLAG_DD | FLAG_CMP;
        if retval != 0 { flags |= FLAG_ERR; }
        d.set16(0, flags);
        d.set16(6, retval);
    }

    fn link_params(&self, d: &mut Desc) {
        let up = self.link_up;
        d.set_param(2, if self.media { self.phy } else { 0xfe });
        d.set_param(3, if up { self.phy_cfg.speeds } else { 0 });
        let media = if self.media { MEDIA_AVAILABLE } else { 0 };
        d.set_param(4, if up { LINK_UP | media } else { media });
        d.set_param(5, 0x01);
        d.set_param(10, if up { self.running_fec() } else { 0 });
    }

    /// Post a link event on the ARQ, if it has room (head short of tail).
    pub fn event(&mut self, up: bool) -> bool {
        self.link_up = up;
        if self.reg(PF_ARQLEN) & LEN_ENABLE == 0 || !self.lse { return false; }
        let count = (self.reg(PF_ARQLEN) & HEAD_MASK) as usize;
        let h = self.reg(PF_ARQH) as usize;
        if h == self.reg(PF_ARQT) as usize { return false; }
        let at = self.base(PF_ARQBAL, PF_ARQBAH) + 32 * h as u64;
        let mut d = Desc([0; 32]);
        self.read(at, &mut d.0);
        assert!(d.flags() & FLAG_BUF != 0 && d.datalen() as usize == ARQ_BUF_SIZE, "ARQ descriptor {h} not armed");
        d.set16(2, OP_GET_LINK_STATUS);
        self.link_params(&mut d);
        d.set16(0, d.flags() | FLAG_DD | FLAG_CMP);
        self.write(at, &d.0);
        self.set(PF_ARQH, ((h + 1) % count) as u32);
        true
    }

    // ---- HMC ----

    /// The host address of FPM offset `fpm`, through SD 0 (paged) and its PD page.
    fn fpm(&self, fpm: u64) -> u64 {
        let sd = *self.sd.get(&0).expect("HMC SD 0 not written");
        let (low, high) = sd;
        assert!(low & hmc::SD_VALID != 0, "HMC SD 0 not valid");
        assert!(low & hmc::SD_DIRECT == 0, "SD 0 not paged");
        assert_eq!((low >> 2) & 0x3ff, 512, "SD 0 backing-page count");
        let pd_page = (high as u64) << 32 | (low & !0xfff) as u64;
        let pd = self.r64(pd_page + 8 * (fpm / 4096));
        assert!(pd & 1 != 0, "PD {} not valid", fpm / 4096);
        (pd & !0xfff) + fpm % 4096
    }

    fn context(&self, tx: bool, q: u32, len: usize) -> Vec<u8> {
        let pf = self.reg(PF_FUNC_RID) & 7;
        let (base, cnt, size) = if tx {
            (self.reg(hmc::glhmc_lantxbase(pf)) as u64 * 512, self.reg(hmc::glhmc_lantxcnt(pf)), 1u64 << self.reg(hmc::GLHMC_LANTXOBJSZ))
        } else {
            (self.reg(hmc::glhmc_lanrxbase(pf)) as u64 * 512, self.reg(hmc::glhmc_lanrxcnt(pf)), 1u64 << self.reg(hmc::GLHMC_LANRXOBJSZ))
        };
        assert!(q < cnt, "queue {q} beyond the {} HMC objects", cnt);
        let mut c = vec![0u8; len];
        self.read(self.fpm(base + q as u64 * size), &mut c);
        c
    }

    fn enable_tx(&mut self, q: u32) -> Ctx {
        let c = self.context(true, q, 128);
        let f = |f: (usize, usize)| get(&c, f.0, f.1);
        assert_eq!(f(hmc::tx::NEW_CONTEXT), 1);
        assert_eq!(f(hmc::tx::RDYLIST), QSET as u64, "TX context scheduled on the VSI's queue set");
        assert_eq!(f(hmc::tx::HEAD_WB_ENA), 0);
        assert_eq!(f(hmc::tx::FC_ENA), 0);
        let ctl = self.reg(qtx_ctl(q));
        assert_eq!(ctl & 3, QTX_CTL_PF_QUEUE, "QTX_CTL: PF queue");
        assert_eq!((ctl >> 2) & 0xf, self.reg(PF_FUNC_RID) & 7, "QTX_CTL: PF index");
        let abs = FIRSTQ + q;
        let pre = self.ops.iter().rev().find(|o| o.0 == gllan_txpre_qdis(abs / 128)).expect("no TXPRE_QDIS");
        assert_eq!(pre.1 & TXPRE_QINDX_MASK, abs % 128);
        assert!(pre.1 & TXPRE_CLEAR_QDIS != 0, "TX pre-disable not cleared before enable");
        Ctx { base: f(hmc::tx::BASE) * 128, qlen: f(hmc::tx::QLEN) }
    }

    fn enable_rx(&mut self, q: u32) -> Ctx {
        let c = self.context(false, q, 32);
        let f = |f: (usize, usize)| get(&c, f.0, f.1);
        assert_eq!(f(hmc::rx::DSIZE), 1, "32-byte descriptors");
        assert_eq!(f(hmc::rx::CRCSTRIP), 1);
        assert_eq!(f(hmc::rx::DBUFF) * 128, BUF_SIZE as u64);
        assert_eq!(f(hmc::rx::DTYPE), 0);
        assert_eq!(f(hmc::rx::HSPLIT_0), 0);
        assert_eq!(f(hmc::rx::FC_ENA), 0);
        assert!(f(hmc::rx::RXMAX) >= 1518);
        Ctx { base: f(hmc::rx::BASE) * 128, qlen: f(hmc::rx::QLEN) }
    }

    // ---- Queues ----

    fn q(&self) -> u32 { self.vsi_queue as u32 }

    /// Write-back on ITR is armed for queue `q`: the vector-0 list starts at
    /// its receive queue, which is linked to its transmit queue, both with
    /// their cause enabled, and DYN_CTL0 has WB_ON_ITR.
    pub fn wb_on_itr(&self) -> bool {
        let q = self.q();
        self.reg(PFINT_DYN_CTL0) & DYN_CTL0_WB_ON_ITR != 0
            && self.reg(PFINT_LNKLST0) == q
            && self.reg(qint_rqctl(q)) & QINT_CAUSE_ENA != 0
            && (self.reg(qint_rqctl(q)) >> QINT_NEXTQ_INDX_SHIFT) & 0x7ff == q
            && (self.reg(qint_rqctl(q)) >> QINT_NEXTQ_TYPE_SHIFT) & 3 == QTYPE_TX
            && self.reg(qint_tqctl(q)) & QINT_CAUSE_ENA != 0
    }

    pub fn flush(&mut self) {
        for (d, qw1) in std::mem::take(&mut self.rx_wb) { self.w64(d + 8, qw1); }
        for d in std::mem::take(&mut self.tx_wb) {
            let qw1 = self.r64(d + 8);
            self.w64(d + 8, qw1 | 0xf);
        }
    }

    fn maybe_flush(&mut self) {
        if self.rx_wb.len() >= 4 {
            for (d, qw1) in self.rx_wb.drain(..4).collect::<Vec<_>>() { self.w64(d + 8, qw1); }
        }
        if self.tx_wb.len() >= 4 {
            for d in self.tx_wb.drain(..4).collect::<Vec<_>>() {
                let qw1 = self.r64(d + 8);
                self.w64(d + 8, qw1 | 0xf);
            }
        }
    }

    pub fn process_tx(&mut self) {
        let q = self.q();
        let Some(ctx) = self.tx_ctx else { return };
        if self.reg(qtx_ena(q)) & QENA_STAT == 0 || !self.link_up { return; }
        let n = ctx.qlen as usize;
        while self.tx_head != self.reg(qtx_tail(q)) as usize {
            let d = ctx.base + 16 * self.tx_head as u64;
            let addr = self.r64(d);
            let qw1 = self.r64(d + 8);
            assert_eq!(qw1 & 0xf, 0, "data descriptor");
            assert_eq!((qw1 >> 4) & 0x3ff, 0x7, "EOP|RS|ICRC");
            let len = ((qw1 >> 34) & 0x3fff) as usize;
            assert!((60..=1518).contains(&len), "TX length {len}");
            let mut frame = vec![0u8; len];
            self.read(addr, &mut frame);
            self.tx_wb.push_back(d);
            self.tx_head = (self.tx_head + 1) % n;
            self.set(qtx_head(q), self.tx_head as u32);
            if self.loopback { self.deliver(&frame); }
            self.wire.push(frame);
        }
        self.maybe_flush();
    }

    /// Receive one frame into the ring, or drop it (filters, queue off, no
    /// descriptor).
    pub fn deliver(&mut self, frame: &[u8]) {
        let q = self.q();
        let dst = &frame[..6];
        let bcast = dst == [0xff; 6];
        let mcast = !bcast && dst[0] & 1 != 0;
        let passes = self.promisc.0
            || (bcast && self.promisc.2)
            || (mcast && self.promisc.1)
            || (!bcast && !mcast && self.filters.iter().any(|f| f == dst));
        let Some(ctx) = self.rx_ctx else { self.dropped += 1; return };
        let n = ctx.qlen as usize;
        if !passes || self.reg(qrx_ena(q)) & QENA_STAT == 0 || self.rx_head == self.reg(qrx_tail(q)) as usize {
            self.dropped += 1;
            return;
        }
        let d = ctx.base + 32 * self.rx_head as u64;
        let addr = self.r64(d);
        assert_eq!(self.r64(d + 8) & 1, 0, "device given a descriptor still holding a frame");
        assert!(frame.len() <= BUF_SIZE);
        self.write(addr, frame);
        // The wire's CRC is stripped (context CRCSTRIP): the length is the frame's.
        self.rx_wb.push_back((d, 1 | 2 | (frame.len() as u64) << 38));
        self.rx_head = (self.rx_head + 1) % n;
        self.maybe_flush();
    }

    fn queue_write(&mut self, r: u32, v: u32) -> u32 {
        let q = self.q();
        if self.stuck == Some(r) { return v & !QENA_STAT | (self.reg(r) & QENA_STAT); }
        if v & QENA_REQ != 0 {
            if r == qrx_ena(q) && self.reg(r) & QENA_STAT == 0 { self.rx_ctx = Some(self.enable_rx(q)); self.rx_head = 0; }
            if r == qtx_ena(q) && self.reg(r) & QENA_STAT == 0 { self.tx_ctx = Some(self.enable_tx(q)); self.tx_head = 0; }
            v | QENA_STAT
        } else {
            v & !QENA_STAT
        }
    }

    fn pf_reset(&mut self) {
        self.pf_resets += 1;
        let keep: Vec<u32> = vec![GLGEN_RSTCTL, GLNVM_ULD, PF_FUNC_RID, PFGEN_PORTNUM, PFLAN_QALLOC,
            hmc::GLHMC_LANQMAX, hmc::GLHMC_LANTXOBJSZ, hmc::GLHMC_LANRXOBJSZ,
            PF_ATQBAL, PF_ATQBAH, PF_ATQLEN, PF_ATQH, PF_ATQT, PF_ARQBAL, PF_ARQBAH, PF_ARQLEN, PF_ARQH, PF_ARQT];
        self.regs.retain(|r, _| keep.contains(r) || (0x000c_6200..0x000c_6a00).contains(r));
        self.sd.clear();
        self.rx_ctx = None;
        self.tx_ctx = None;
        self.rx_wb.clear();
        self.tx_wb.clear();
    }
}

impl Nic {
    /// A PFHMC_SDCMD write: store the SD the data registers hold.
    fn sd_write(&mut self) {
        let cmd = self.reg(hmc::PFHMC_SDCMD);
        assert!(cmd & hmc::SDCMD_WRITE != 0);
        let idx = cmd & 0xfff;
        let (low, high) = (self.reg(hmc::PFHMC_SDDATALOW), self.reg(hmc::PFHMC_SDDATAHIGH));
        self.sd.insert(idx, (low, high));
    }
}

impl Bus for Nic {
    type Error = &'static str;
    fn r32(&mut self, r: u32) -> Result<u32, Self::Error> {
        if self.removed { return Ok(u32::MAX); }
        if r == GLGEN_RSTAT && self.global_reset_polls > 0 {
            self.global_reset_polls -= 1;
            return Ok(1);
        }
        if r == PFGEN_CTRL && self.reg(r) & PFGEN_CTRL_PFSWR != 0 {
            if self.pfswr_polls == usize::MAX { return Ok(PFGEN_CTRL_PFSWR); }
            if self.pfswr_polls > 0 { self.pfswr_polls -= 1; return Ok(PFGEN_CTRL_PFSWR); }
            self.pf_reset();
            self.set(PFGEN_CTRL, 0);
            return Ok(0);
        }
        Ok(self.reg(r))
    }
    fn w32(&mut self, r: u32, v: u32) -> Result<(), Self::Error> {
        if self.removed { return Ok(()); }
        self.ops.push((r, v));
        let q = self.q();
        let v = if r == qrx_ena(q) || r == qtx_ena(q) { self.queue_write(r, v) } else { v };
        if r == qrx_tail(q) {
            assert!(v as usize % 8 == 0, "receive tail {v} not a multiple of 8");
            self.rx_tails.push(v);
        }
        self.set(r, v);
        match r {
            PF_ATQT => self.run_atq(),
            hmc::PFHMC_SDCMD => self.sd_write(),
            GLLAN_RCTL_0 => { self.set(r, 0); }
            _ => {}
        }
        if r == qtx_tail(q) { self.process_tx(); }
        Ok(())
    }
    fn delay_us(&mut self, _: usize) {
        self.delays += 1;
        if let Some(t) = self.an_timer {
            if t <= 1 { self.an_timer = None; self.link_up = self.can_link(); } else { self.an_timer = Some(t - 1); }
        }
        for f in std::mem::take(&mut self.inbound) { self.deliver(&f); }
        self.process_tx();
        if self.wb_on_itr() { self.flush(); }
    }
}

/// A zeroed DMA region, the simulated device on it, and the region's view.
pub fn setup() -> (Vec<u8>, Nic, Dma) {
    let mut mem = vec![0xa5u8; DMA_BYTES];
    let nic = Nic::new(&mut mem);
    let dma = Dma { host: mem.as_mut_ptr(), device: DEVICE };
    (mem, nic, dma)
}

pub fn frame(dst: [u8; 6], seq: u16, len: usize) -> Vec<u8> {
    let mut f = vec![0u8; len];
    f[..6].copy_from_slice(&dst);
    f[6..12].copy_from_slice(&[0x02, 0, 0, 0, 0, 9]);
    f[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
    f[14..16].copy_from_slice(&seq.to_be_bytes());
    f
}
