//! The PF bring-up, admin queue, HMC and queue pair against the simulated
//! device in `sim.rs`.
#[path = "../src/i40e.rs"]
#[allow(dead_code)]
mod i40e;
#[allow(dead_code)]
mod sim;
use i40e::adminq::*;
use i40e::hmc::{self, get};
use i40e::ring;
use i40e::*;
use i40e::port::*;
use sim::*;

type E = &'static str;

/// Run the whole bring-up; the steps it logged.
fn bring(nic: &mut Nic, dma: Dma) -> (R<Up<E>, E>, Vec<Step<E>>, AdminQ) {
    let id = identify(nic).unwrap();
    let mut aq = AdminQ::new(dma);
    let mut log = Vec::new();
    let r = bring_up(nic, &mut aq, dma, &id, 4000, &mut |s| log.push(s));
    (r, log, aq)
}

fn up() -> (Vec<u8>, Nic, Up<E>, AdminQ) {
    let (mem, mut nic, dma) = setup();
    let (r, _, aq) = bring(&mut nic, dma);
    (mem, nic, r.unwrap(), aq)
}

#[test]
fn identify_reads_pf_port_and_queues() {
    let (_mem, mut nic, _) = setup();
    let id = identify(&mut nic).unwrap();
    assert_eq!(id, Identity { pf: PF, port: 1, base_queue: FIRSTQ, queues: LASTQ - FIRSTQ + 1 });
    nic.set(PFLAN_QALLOC, 0);
    assert_eq!(identify(&mut nic), Err(Error::NoQueues { qalloc: 0 }));
    nic.removed = true;
    assert_eq!(identify(&mut nic), Err(Error::Removed));
}

#[test]
fn bring_up_runs_every_step_in_order() {
    let (_mem, mut nic, dma) = setup();
    let (r, log, _) = bring(&mut nic, dma);
    let up = r.unwrap();
    assert_eq!(up.net.mac, MAC);
    assert!(up.link.up);
    assert_eq!(up.link.mbps, 10_000);
    assert_eq!((up.version.fw_major, up.version.api_major, up.version.api_minor), (9, 1, 15));
    assert_eq!(
        nic.commands,
        vec![OP_GET_VERSION, OP_CLEAR_PXE, OP_MAC_ADDRESS_READ, OP_GET_SWITCH_CONFIG, OP_GET_SWITCH_CONFIG,
            OP_GET_VSI_PARAMS, OP_ADD_MACVLAN, OP_GET_LINK_STATUS, OP_GET_PHY_ABILITIES]
    );
    assert_eq!(nic.pf_resets, 1);
    assert!(!nic.pxe);
    assert!(nic.wrote(GLLAN_RCTL_0).contains(&1), "PXE-mode bit written after Clear PXE");
    assert_eq!(nic.filters, vec![MAC, MAC], "MAC filter for any VLAN added beside the firmware's");
    assert!(matches!(log[0], Step::Reset(Reset { global: false, .. })));
    assert_eq!(log[2], Step::Pxe(true));
    assert_eq!(log[3], Step::Vsi { mac: MAC, vsi: Vsi { seid: VSI_SEID, queue: 0, qset: QSET } });
    assert_eq!(log[4], Step::MacFilter(Ok(true)));
    assert!(matches!(log[6], Step::Fec { fec: PortFec { standard: Fec::None, set: FecSet::Kept, .. }, .. }), "{:?}", log[6]);
    assert!(matches!(log[7], Step::Link { waited_ms: None, .. }));
    assert!(matches!(log[8], Step::Check(Some(_))), "{:?}", log[8]);
    assert_eq!(nic.wire.len(), 1, "the DMA check frame went out");
    assert_eq!(nic.wire[0], ring::check_frame(MAC).to_vec());
    // The queues are stopped again.
    assert_eq!(nic.reg(qtx_ena(0)) & QENA_STAT, 0);
    assert_eq!(nic.reg(qrx_ena(0)) & QENA_STAT, 0);
}

#[test]
fn clear_quiets_interrupts_and_every_pf_queue_before_the_reset() {
    let (_mem, mut nic, dma) = setup();
    bring(&mut nic, dma).0.unwrap();
    let reset = nic.pos((PFGEN_CTRL, PFGEN_CTRL_PFSWR));
    assert!(nic.pos((PFINT_ICR0_ENA, 0)) < reset);
    assert!(nic.pos((PFINT_LNKLST0, LNKLST_EOL)) < reset);
    for q in 0..(LASTQ - FIRSTQ + 1) {
        let abs = FIRSTQ + q;
        assert!(nic.pos((gllan_txpre_qdis(abs / 128), abs % 128 | TXPRE_SET_QDIS)) < nic.pos((qtx_ena(q), 0)));
        assert!(nic.pos((qrx_ena(q), 0)) < reset);
    }
}

#[test]
fn pf_reset_waits_out_a_global_reset_and_then_skips_its_own() {
    let (_mem, mut nic, _) = setup();
    nic.global_reset_polls = 3;
    let r = pf_reset(&mut nic).unwrap();
    assert!(r.global);
    assert_eq!(nic.pf_resets, 0);
    assert!(nic.wrote(PFGEN_CTRL).is_empty());
}

#[test]
fn pf_reset_failures_name_their_stage() {
    let (_mem, mut nic, _) = setup();
    nic.global_reset_polls = usize::MAX;
    assert_eq!(pf_reset(&mut nic), Err(Error::Reset { stage: ResetStage::Global, value: 1 }));
    let (_mem, mut nic, _) = setup();
    nic.set(GLNVM_ULD, ULD_CORE_DONE);
    assert_eq!(pf_reset(&mut nic), Err(Error::Reset { stage: ResetStage::Firmware, value: ULD_CORE_DONE }));
    let (_mem, mut nic, _) = setup();
    nic.pfswr_polls = usize::MAX;
    assert_eq!(pf_reset(&mut nic), Err(Error::Reset { stage: ResetStage::Pf, value: PFGEN_CTRL_PFSWR }));
}

#[test]
fn admin_queue_registers_point_at_the_rings() {
    let (_mem, mut nic, dma) = setup();
    let mut aq = AdminQ::new(dma);
    aq.init(&mut nic).unwrap();
    assert_eq!(nic.reg(PF_ATQBAL), (DEVICE + ATQ as u64) as u32);
    assert_eq!(nic.reg(PF_ATQBAH), ((DEVICE + ATQ as u64) >> 32) as u32);
    assert_eq!(nic.reg(PF_ATQLEN), ATQ_COUNT as u32 | LEN_ENABLE);
    assert_eq!(nic.reg(PF_ARQBAL), (DEVICE + ARQ as u64) as u32);
    assert_eq!(nic.reg(PF_ARQLEN), ARQ_COUNT as u32 | LEN_ENABLE);
    assert_eq!(nic.reg(PF_ARQT), ARQ_COUNT as u32 - 1);
    // Disabled before being programmed: a previous driver's rings are dropped first.
    assert!(nic.pos((PF_ATQLEN, 0)) < nic.pos((PF_ATQLEN, ATQ_COUNT as u32 | LEN_ENABLE)));
    // Commands wrap around the ring.
    for _ in 0..ATQ_COUNT + 3 { aq.version(&mut nic).unwrap(); }
    assert_eq!(nic.reg(PF_ATQH), 3);
}

#[test]
fn admin_errors_timeouts_and_api_are_reported() {
    let (_mem, mut nic, dma) = setup();
    let mut aq = AdminQ::new(dma);
    aq.init(&mut nic).unwrap();
    nic.refuse = Some((OP_GET_VSI_PARAMS, 14));
    assert_eq!(aq.vsi(&mut nic, VSI_SEID), Err(Error::Admin { opcode: OP_GET_VSI_PARAMS, retval: 14 }));
    nic.refuse = None;
    nic.api = (2, 0);
    assert_eq!(aq.version(&mut nic), Err(Error::AdminApi { major: 2, minor: 0 }));
    nic.hang = Some(OP_GET_LINK_STATUS);
    let before = nic.delays;
    assert!(matches!(aq.link(&mut nic, false), Err(Error::AdminTimeout { opcode: OP_GET_LINK_STATUS, .. })));
    assert!(nic.delays - before >= TIMEOUT_POLLS, "waited the whole 250 ms");
}

#[test]
fn admin_setup_fails_if_the_base_does_not_stick() {
    struct Deaf<'a>(&'a mut Nic);
    impl Bus for Deaf<'_> {
        type Error = E;
        fn r32(&mut self, r: u32) -> Result<u32, E> { if r == PF_ATQBAL { Ok(0) } else { self.0.r32(r) } }
        fn w32(&mut self, r: u32, v: u32) -> Result<(), E> { self.0.w32(r, v) }
        fn delay_us(&mut self, us: usize) { self.0.delay_us(us) }
    }
    let (_mem, mut nic, dma) = setup();
    let mut aq = AdminQ::new(dma);
    assert_eq!(aq.init(&mut Deaf(&mut nic)), Err(Error::AdminSetup { bal: 0 }));
}

#[test]
fn clear_pxe_twice_is_already_clear() {
    let (_mem, mut nic, dma) = setup();
    let mut aq = AdminQ::new(dma);
    aq.init(&mut nic).unwrap();
    assert_eq!(aq.clear_pxe(&mut nic), Ok(true));
    assert_eq!(aq.clear_pxe(&mut nic), Ok(false));
    assert_eq!(nic.wrote(GLLAN_RCTL_0), vec![1, 1]);
}

#[test]
fn switch_config_is_read_page_by_page_and_needs_a_vsi() {
    let (_mem, mut nic, dma) = setup();
    let mut aq = AdminQ::new(dma);
    aq.init(&mut nic).unwrap();
    nic.switch_pages = vec![vec![(1, 2)], vec![(17, 0x200)], vec![(SW_ELEM_VSI, VSI_SEID)]];
    assert_eq!(aq.find_vsi(&mut nic), Ok(VSI_SEID));
    assert_eq!(nic.commands.iter().filter(|c| **c == OP_GET_SWITCH_CONFIG).count(), 3);
    nic.switch_pages = vec![vec![(1, 2)], vec![(17, 0x200)]];
    assert_eq!(aq.find_vsi(&mut nic), Err(Error::NoVsi));
}

#[test]
fn mac_filter_already_there_is_fine() {
    let (_mem, mut nic, dma) = setup();
    let mut aq = AdminQ::new(dma);
    aq.init(&mut nic).unwrap();
    assert_eq!(aq.add_mac(&mut nic, VSI_SEID, MAC), Ok(true));
    assert_eq!(aq.add_mac(&mut nic, VSI_SEID, MAC), Ok(false));
}

#[test]
fn a_refused_mac_filter_does_not_stop_bring_up() {
    let (_mem, mut nic, dma) = setup();
    nic.refuse = Some((OP_ADD_MACVLAN, 16));
    let (r, log, _) = bring(&mut nic, dma);
    assert!(r.is_ok());
    assert!(log.contains(&Step::MacFilter(Err(Error::Admin { opcode: OP_ADD_MACVLAN, retval: 16 }))));
}

#[test]
fn hmc_is_one_paged_sd_with_one_backing_page() {
    let (_mem, mut nic, dma) = setup();
    let l = hmc::setup(&mut nic, &dma, PF, 0).unwrap();
    assert_eq!(l, hmc::Layout { tx_size: 128, rx_base: 512, rx_size: 32, count: 1 });
    assert_eq!(nic.reg(hmc::glhmc_lantxbase(PF)), 0);
    assert_eq!(nic.reg(hmc::glhmc_lantxcnt(PF)), 1);
    assert_eq!(nic.reg(hmc::glhmc_lanrxbase(PF)), 1);
    assert_eq!(nic.reg(hmc::glhmc_lanrxcnt(PF)), 1);
    assert_eq!(nic.reg(hmc::glhmc_fcoeddpcnt(PF)), 0);
    assert_eq!(nic.reg(hmc::glhmc_fcoefcnt(PF)), 0);
    let (low, high) = nic.sd[&0];
    assert_eq!(high, (DEVICE >> 32) as u32);
    assert_eq!(low, (DEVICE + HMC_PD as u64) as u32 | 512 << 2 | 1, "paged, 512 PDs, valid");
    assert_eq!(nic.r64(DEVICE + HMC_PD as u64), DEVICE + HMC_BP as u64 | 1);
    assert_eq!(nic.r64(DEVICE + HMC_PD as u64 + 8), 0, "one PD only");
    assert!(nic.pos((hmc::PFHMC_SDCMD, hmc::SDCMD_WRITE)) < nic.pos((hmc::PFHMC_PDINV, 0)));
}

#[test]
fn hmc_layout_follows_the_vsi_queue_and_its_limits() {
    let (_mem, mut nic, dma) = setup();
    let l = hmc::setup(&mut nic, &dma, PF, 5).unwrap();
    assert_eq!((l.count, l.tx(5), l.rx_base, l.rx(5)), (6, 640, 1024, 1024 + 160));
    assert_eq!(hmc::setup(&mut nic, &dma, PF, 40), Err(Error::QueueRange { queue: 40 }));
    nic.set(hmc::GLHMC_LANQMAX, 2);
    assert!(matches!(hmc::setup(&mut nic, &dma, PF, 5), Err(Error::Hmc { qmax: 2, .. })));
    let odd = Dma { host: dma.host, device: DEVICE + 0x800 };
    assert_eq!(hmc::setup(&mut nic, &odd, PF, 0), Err(Error::Unaligned { device: DEVICE + 0x800 }));
}

#[test]
fn context_fields_land_on_the_shared_code_bit_positions() {
    let t = hmc::tx_context(0x1_2345_6780, 32, 0x2ab);
    assert_eq!(get(&t, 30, 1), 1, "new context");
    assert_eq!(get(&t, 32, 57), 0x1_2345_6780 / 128);
    assert_eq!(get(&t, 161, 13), 32, "qlen in line 1");
    assert_eq!(get(&t, 980, 10), 0x2ab, "rdylist in line 7");
    let r = hmc::rx_context(0x1_2345_6800, 32, 2048, 1522);
    assert_eq!(get(&r, 32, 57), 0x1_2345_6800 / 128);
    assert_eq!(get(&r, 89, 13), 32);
    assert_eq!(get(&r, 102, 7), 16);
    assert_eq!(get(&r, 116, 1), 1);
    assert_eq!(get(&r, 117, 1), 1);
    assert_eq!(get(&r, 174, 14), 1522);
    assert_eq!(get(&r, 201, 1), 1);
    // put/get across byte boundaries, and clearing.
    let mut c = [0xffu8; 16];
    hmc::put(&mut c, 5, 13, 0);
    assert_eq!(get(&c, 5, 13), 0);
    assert_eq!(get(&c, 0, 5), 0x1f);
    assert_eq!(get(&c, 18, 6), 0x3f);
}

#[test]
fn link_down_restarts_autonegotiation_and_waits() {
    let (_mem, mut nic, dma) = setup();
    nic.link_up = false;
    let (r, log, _) = bring(&mut nic, dma);
    let up = r.unwrap();
    assert!(up.link.up);
    assert_eq!(nic.an_restarts, 1);
    assert!(log.iter().any(|s| matches!(s, Step::Link { waited_ms: Some(300), .. })), "{log:?}");
}

#[test]
fn link_that_stays_down_skips_the_dma_check() {
    let (_mem, mut nic, dma) = setup();
    nic.link_up = false;
    nic.an_link_after = None;
    let (r, log, _) = bring(&mut nic, dma);
    assert!(!r.unwrap().link.up);
    assert!(log.iter().any(|s| matches!(s, Step::Link { waited_ms: Some(4000), .. })));
    assert!(!log.iter().any(|s| matches!(s, Step::Check(_))));
    assert!(nic.wire.is_empty());
}

#[test]
fn queue_start_programs_contexts_writeback_and_tails() {
    let (_mem, mut nic, up, _) = up();
    let mut net = up.net;
    net.start(&mut nic).unwrap();
    // The simulator checked both contexts as the queues were enabled.
    let rx = nic.rx_ctx.unwrap();
    let tx = nic.tx_ctx.unwrap();
    assert_eq!((rx.base, rx.qlen), (DEVICE + RX_RING as u64, RX_COUNT as u64));
    assert_eq!((tx.base, tx.qlen), (DEVICE + TX_RING as u64, TX_COUNT as u64));
    assert!(nic.wb_on_itr());
    assert_eq!(nic.reg(pfint_itr0(0)), ITR_WRITEBACK);
    assert_eq!(nic.reg(PFINT_DYN_CTL0) & 1, 0, "the interrupt itself stays disabled");
    assert_eq!(nic.reg(PFINT_ICR0_ENA), 0);
    assert_eq!(nic.reg(qrx_tail(0)), (RX_COUNT - 8) as u32);
    // Receive enabled before transmit.
    assert!(nic.pos((qrx_ena(0), QENA_REQ)) < nic.pos((qtx_ena(0), QENA_REQ)));
}

#[test]
fn one_frame_is_seen_at_once_thanks_to_writeback_on_itr() {
    let (_mem, mut nic, up, _) = up();
    let mut net = up.net;
    net.start(&mut nic).unwrap();
    nic.deliver(&frame(MAC, 1, 60));
    assert_eq!(net.peek(&mut nic).unwrap(), None, "written back in a batch, not yet");
    nic.delay_us(1);
    assert_eq!(net.peek(&mut nic).unwrap(), Some((60, MAC)));
    let mut out = [0u8; 2048];
    assert_eq!(net.receive(&mut nic, &mut out).unwrap(), Some(60));
    assert_eq!(&out[..60], &frame(MAC, 1, 60)[..]);
}

#[test]
fn loopback_traffic_wraps_the_rings_many_times() {
    let (_mem, mut nic, up, _) = up();
    let mut net = up.net;
    net.start(&mut nic).unwrap();
    nic.loopback = true;
    let mut out = [0u8; 2048];
    for seq in 0..200u16 {
        let f = frame(MAC, seq, 60 + (seq as usize % 900));
        assert!(net.transmit(&mut nic, &f).unwrap());
        nic.delay_us(1);
        let n = net.receive(&mut nic, &mut out).unwrap().expect("looped back");
        assert_eq!(&out[..n], &f[..]);
        net.reclaim();
        assert_eq!(net.tx_pending(), 0);
    }
    assert_eq!(nic.dropped, 0, "the device never ran out of receive descriptors");
    assert!(nic.rx_tails.len() > 20, "the receive tail moved in batches");
}

#[test]
fn transmit_pads_short_frames_and_refuses_bad_lengths() {
    let (_mem, mut nic, up, _) = up();
    let mut net = up.net;
    net.start(&mut nic).unwrap();
    assert!(net.transmit(&mut nic, &frame([0xff; 6], 1, 20)).unwrap());
    assert_eq!(nic.wire.last().unwrap().len(), 60);
    assert_eq!(net.transmit(&mut nic, &[0; 13]), Err(Error::Frame { len: 13 }));
    assert_eq!(net.transmit(&mut nic, &[0; 1519]), Err(Error::Frame { len: 1519 }));
}

#[test]
fn transmit_ring_full_without_link() {
    let (_mem, mut nic, up, _) = up();
    let mut net = up.net;
    net.start(&mut nic).unwrap();
    nic.link_up = false;
    for i in 0..TX_COUNT - 1 { assert!(net.transmit(&mut nic, &frame([0xff; 6], i as u16, 60)).unwrap()); }
    assert!(!net.transmit(&mut nic, &frame([0xff; 6], 99, 60)).unwrap());
}

#[test]
fn bad_receive_descriptors_are_skipped() {
    let (_mem, mut nic, up, _) = up();
    let mut net = up.net;
    net.start(&mut nic).unwrap();
    nic.deliver(&frame(MAC, 1, 60));
    nic.deliver(&frame(MAC, 2, 60));
    nic.flush();
    // Mark the first one with a receive error.
    let d = DEVICE + RX_RING as u64;
    let qw1 = nic.r64(d + 8);
    nic.w64(d + 8, qw1 | ring::RX_ERR_RXE);
    let mut out = [0u8; 2048];
    assert_eq!(net.receive(&mut nic, &mut out).unwrap(), Some(60));
    assert_eq!(&out[14..16], &2u16.to_be_bytes());
    assert_eq!(net.rx_errors, 1);
}

#[test]
fn a_frame_too_long_for_the_buffer_stays_queued() {
    let (_mem, mut nic, up, _) = up();
    let mut net = up.net;
    net.start(&mut nic).unwrap();
    nic.deliver(&frame(MAC, 1, 600));
    nic.flush();
    let mut small = [0u8; 100];
    assert_eq!(net.receive(&mut nic, &mut small), Err(Error::Frame { len: 600 }));
    let mut out = [0u8; 2048];
    assert_eq!(net.receive(&mut nic, &mut out).unwrap(), Some(600));
}

#[test]
fn queue_stop_pre_disables_and_waits() {
    let (_mem, mut nic, up, _) = up();
    let mut net = up.net;
    net.start(&mut nic).unwrap();
    nic.ops.clear();
    net.stop(&mut nic).unwrap();
    let pre = nic.pos((gllan_txpre_qdis(FIRSTQ / 128), FIRSTQ % 128 | TXPRE_SET_QDIS));
    assert!(pre < nic.first(qtx_ena(0)));
    assert_eq!(nic.reg(qtx_ena(0)) & QENA_STAT, 0);
    assert_eq!(nic.reg(qrx_ena(0)) & QENA_STAT, 0);
    assert!(!nic.wb_on_itr());
    assert_eq!(nic.reg(PFINT_LNKLST0), LNKLST_EOL);
}

#[test]
fn a_queue_that_will_not_switch_is_a_timeout() {
    let (_mem, mut nic, up, _) = up();
    let mut net = up.net;
    nic.stuck = Some(qtx_ena(0));
    assert!(matches!(net.start(&mut nic), Err(Error::QueueTimeout { tx: true, .. })));
    assert!(!net.running);
    nic.stuck = None;
    net.start(&mut nic).unwrap();
    nic.stuck = Some(qrx_ena(0));
    assert!(matches!(net.stop(&mut nic), Err(Error::QueueTimeout { tx: false, .. })));
}

#[test]
fn link_events_are_drained_and_the_queue_rearmed() {
    let (_mem, mut nic, _up, mut aq) = up();
    assert_eq!(aq.events(&mut nic), Ok(None));
    aq.link(&mut nic, true).unwrap();
    for round in 0..3 {
        // Many events, more than the ring holds over the rounds.
        assert!(nic.event(false));
        assert!(nic.event(true));
        assert!(nic.event(false));
        let l = aq.events(&mut nic).unwrap().expect("an event");
        assert!(!l.up, "round {round}: the last event wins");
    }
    assert!(nic.event(true));
    assert_eq!(aq.events(&mut nic).unwrap().map(|l| l.up), Some(true));
}

#[test]
fn release_turns_off_the_admin_queue_and_resets() {
    let (_mem, mut nic, up, mut aq) = up();
    let mut net = up.net;
    net.start(&mut nic).unwrap();
    net.stop(&mut nic).unwrap();
    let resets = nic.pf_resets;
    release(&mut nic, &mut aq).unwrap();
    assert!(nic.shutdown);
    assert_eq!(nic.reg(PF_ATQLEN), 0);
    assert_eq!(nic.reg(PF_ARQLEN), 0);
    assert_eq!(nic.reg(PF_ATQBAL), 0);
    assert_eq!(nic.pf_resets, resets + 1);
    assert!(nic.sd.is_empty(), "the reset dropped the HMC segment descriptor");
    // Before the reset, the SD was invalidated by hand too.
    let sd_off = nic.last((hmc::PFHMC_SDDATALOW, 512 << 2));
    assert!(sd_off < nic.last((PFGEN_CTRL, PFGEN_CTRL_PFSWR)));
}

#[test]
fn release_after_a_failed_bring_up_still_resets() {
    let (_mem, mut nic, dma) = setup();
    nic.hang = Some(OP_MAC_ADDRESS_READ);
    let (r, _, mut aq) = bring(&mut nic, dma);
    assert!(matches!(r, Err(Error::AdminTimeout { opcode: OP_MAC_ADDRESS_READ, .. })));
    // The firmware still hangs: shutdown goes unanswered, the rest happens.
    assert!(release(&mut nic, &mut aq).is_ok());
    assert_eq!(nic.reg(PF_ATQLEN), 0);
    assert_eq!(nic.pf_resets, 2);
}

#[test]
fn speeds_and_link_display() {
    assert_eq!(speed_mbps(0x08), 10_000);
    assert_eq!(speed_mbps(0x10), 40_000);
    assert_eq!(speed_mbps(0x40), 25_000);
    assert_eq!(speed_mbps(0x00), 0);
    assert_eq!(Link { up: true, mbps: 25_000, ..Default::default() }.to_string(), "up 25 Gb/s");
    assert_eq!(Link { up: true, mbps: 2_500, ..Default::default() }.to_string(), "up 2500 Mb/s");
    assert_eq!(Link::default().to_string(), "down");
}

// ---- The port: standard FEC per media, one line (stormnic-i40e#5) ----

const RS: u8 = LINK_FEC_RS;
const KR: u8 = LINK_FEC_KR;

fn line(up: &Up<E>, detail: Option<&dyn core::fmt::Display>) -> String {
    PortLine {
        version: "0.2.0", at: "0000:01:00.1", device: 0x158b, part: "XXV710", port: 1, mac: MAC, link: up.link,
        fec: &up.fec, waited_ms: up.waited_ms, detail,
    }
    .to_string()
}

fn sets(nic: &Nic) -> usize { nic.commands.iter().filter(|c| **c == OP_SET_PHY_CONFIG).count() }

#[test]
fn the_standard_fec_per_media_and_speed() {
    for phy in 0x1f..=0x24u8 { assert_eq!(standard_fec(phy), Fec::Rs, "25G PHY type {phy:#x}"); }
    for phy in [0x00u8, 0x01, 0x03, 0x04, 0x07, 0x0a, 0x0b, 0x12, 0x13, 0x14, 0x15, 0x16, 0x18, 0x19, 0x1a, 0x1b, 0x1e, 0x26, 0x27] {
        assert_eq!(standard_fec(phy), Fec::None, "PHY type {phy:#x}");
    }
    assert_eq!(standard_fec(PHY_EMPTY), Fec::None);
    assert_eq!(standard_fec(PHY_UNSUPPORTED), Fec::None);
    assert_eq!(media(0x21), Some(("25GBASE-SR", 25_000)));
    assert_eq!(media(0x14), Some(("10GBASE-SR", 10_000)));
    assert_eq!(media(0x19), Some(("40GBASE-SR4", 40_000)));
    assert_eq!(media(PHY_UNRECOGNIZED), None);
}

#[test]
fn fec_configuration_bytes() {
    for f in [Fec::None, Fec::BaseR, Fec::Rs, Fec::Auto] { assert_eq!(Fec::of_config(f.config()), f); }
    assert_eq!(Fec::Rs.config(), FEC_ABILITY_RS | FEC_REQUEST_RS);
    assert_eq!(Fec::of_config(FEC_ABILITY_KR | FEC_ABILITY_RS), Fec::Auto);
    assert_eq!(Fec::of_config(FEC_REQUEST_RS), Fec::Rs);
    let l = |config| Link { up: true, config, ..Default::default() };
    assert_eq!((l(0).fec(), l(KR).fec(), l(RS).fec()), (Fec::None, Fec::BaseR, Fec::Rs));
}

#[test]
fn a_10g_port_keeps_no_fec_and_says_so() {
    let (_mem, nic, up, _) = up();
    assert_eq!(up.fec.set, FecSet::Kept);
    assert_eq!(sets(&nic), 0, "nothing written");
    assert_eq!(
        line(&up, None),
        "stormnic-i40e 0.2.0: 0000:01:00.1 8086:158b XXV710 port 1: MAC 02:11:22:33:44:55, media 10GBASE-SR, speed 10 Gb/s, FEC none, link UP"
    );
}

#[test]
fn a_25g_port_already_on_rs_is_left_alone() {
    let (_mem, mut nic, dma) = setup();
    nic.with_media(0x21, RS); // NVM default: FEC auto, partner RS
    let (r, _, _) = bring(&mut nic, dma);
    let up = r.unwrap();
    assert_eq!(up.fec.set, FecSet::Kept);
    assert_eq!(sets(&nic), 0);
    assert_eq!(nic.an_restarts, 0);
    assert!(line(&up, None).ends_with("media 25GBASE-SR, speed 25 Gb/s, FEC rs, link UP"), "{}", line(&up, None));
}

#[test]
fn a_25g_port_down_gets_rs_and_links() {
    let (_mem, mut nic, dma) = setup();
    nic.with_media(0x20, RS);
    nic.phy_cfg.fec = 0; // a previous driver turned FEC off: the RS partner never links
    nic.link_up = false;
    let before = nic.phy_cfg;
    let (r, log, _) = bring(&mut nic, dma);
    let up = r.unwrap();
    assert_eq!(up.fec.set, FecSet::Written);
    assert_eq!(nic.an_restarts, 0, "Set PHY Config restarted the link; no extra Restart AN");
    assert_eq!(nic.set_phy.len(), 1);
    let p = nic.set_phy[0];
    assert_eq!(u32::from_le_bytes([p[0], p[1], p[2], p[3]]), before.phy_types);
    assert_eq!(p[4], before.speeds);
    assert_eq!(p[5], PHY_ENABLE_LINK | PHY_ENABLE_AN | PHY_ATOMIC_LINK, "FEC ability bits are not Set PHY Config's");
    assert_eq!(u32::from_le_bytes([p[8], p[9], p[10], p[11]]), before.eeer);
    assert_eq!(p[13], before.phy_types_ext);
    assert_eq!(p[14], FEC_ABILITY_RS | FEC_REQUEST_RS);
    assert!(up.link.up);
    assert_eq!(up.link.fec(), Fec::Rs);
    assert!(log.iter().any(|s| matches!(s, Step::Link { waited_ms: Some(300), .. })), "{log:?}");
    assert!(log.iter().any(|s| matches!(s, Step::Check(Some(_)))));
    assert!(line(&up, None).ends_with("media 25GBASE-CR, speed 25 Gb/s, FEC rs, link UP"), "{}", line(&up, None));
}

#[test]
fn a_25g_port_on_base_r_is_moved_to_rs() {
    let (_mem, mut nic, dma) = setup();
    nic.with_media(0x21, KR); // auto picked BASE-R with a BASE-R partner
    let (r, _, _) = bring(&mut nic, dma);
    let up = r.unwrap();
    assert_eq!(up.fec.set, FecSet::Written);
    assert_eq!(nic.phy_cfg.fec, FEC_ABILITY_RS | FEC_REQUEST_RS);
    // The partner stays on BASE-R: no link, said as an error.
    assert!(!up.link.up);
    let l = line(&up, None);
    assert!(
        l.ends_with("media 25GBASE-SR, speed none, FEC rs, link DOWN: error: no link after 4000 ms (the link partner must run RS-FEC too, IEEE 802.3 Clause 108; check the cable and the switch port)"),
        "{l}"
    );
}

#[test]
fn a_refused_fec_still_brings_the_port_up_and_says_why() {
    let (_mem, mut nic, dma) = setup();
    nic.with_media(0x21, KR);
    nic.refuse = Some((OP_SET_PHY_CONFIG, 2));
    let (r, _, _) = bring(&mut nic, dma);
    let up = r.unwrap();
    assert_eq!(up.fec.set, FecSet::Refused(Error::Admin { opcode: OP_SET_PHY_CONFIG, retval: 2 }));
    assert!(up.link.up, "the link stays as it was");
    let l = line(&up, Some(&"firmware error 2"));
    assert!(
        l.ends_with("speed 25 Gb/s, FEC base-r, link UP: error: the standard FEC for 25GBASE-SR is rs; FEC not set (Set PHY Config): firmware error 2"),
        "{l}"
    );
}

#[test]
fn an_unread_phy_configuration_is_reported() {
    let (_mem, mut nic, dma) = setup();
    nic.with_media(0x21, RS);
    nic.phy_cfg.fec = 0;
    nic.link_up = false;
    nic.refuse = Some((OP_GET_PHY_ABILITIES, 5));
    let (r, _, _) = bring(&mut nic, dma);
    let up = r.unwrap();
    assert!(matches!(up.fec.set, FecSet::Unread(Error::Admin { opcode: OP_GET_PHY_ABILITIES, .. })));
    assert_eq!(nic.an_restarts, 1, "the link is still restarted");
    let l = line(&up, Some(&"refused"));
    assert!(l.contains("FEC unknown, link DOWN: error: no link after 4000 ms; FEC not read: refused (the link partner"), "{l}");
}

#[test]
fn no_phy_types_writes_nothing() {
    let (_mem, mut nic, dma) = setup();
    nic.with_media(0x21, RS);
    nic.phy_cfg = PhyConfig { fec: 0, ..Default::default() };
    nic.link_up = false;
    let (r, _, _) = bring(&mut nic, dma);
    let up = r.unwrap();
    assert_eq!(up.fec.set, FecSet::NoPhyTypes);
    assert_eq!(sets(&nic), 0);
    assert!(line(&up, None).contains("FEC none, link DOWN: error: no link after 4000 ms; FEC not set: the firmware lists no PHY type"));
}

#[test]
fn no_module_is_an_error_line() {
    let (_mem, mut nic, dma) = setup();
    nic.media = false;
    nic.link_up = false;
    let (r, _, _) = bring(&mut nic, dma);
    let up = r.unwrap();
    assert_eq!(up.fec.set, FecSet::NoMedia);
    assert!(!nic.commands.contains(&OP_GET_PHY_ABILITIES));
    assert!(line(&up, None).ends_with("media none, speed none, FEC unknown, link DOWN: error: no module or cable (the firmware reports no media)"));
}

#[test]
fn an_unsupported_module_is_an_error_line() {
    let (_mem, mut nic, dma) = setup();
    nic.phy = PHY_UNSUPPORTED;
    nic.link_up = false;
    let (r, _, _) = bring(&mut nic, dma);
    let up = r.unwrap();
    assert_eq!(up.fec.set, FecSet::NoMedia);
    assert!(line(&up, None).ends_with("media PHY type 0x0f, speed none, FEC unknown, link DOWN: error: the firmware does not run this media (PHY type 0x0f, unsupported module)"),
        "{}", line(&up, None));
}

#[test]
fn link_events_carry_the_running_fec() {
    let (_mem, mut nic, dma) = setup();
    nic.with_media(0x21, RS);
    let (r, _, mut aq) = bring(&mut nic, dma);
    r.unwrap();
    aq.link(&mut nic, true).unwrap();
    assert!(nic.event(true));
    let l = aq.events(&mut nic).unwrap().unwrap();
    assert_eq!((l.mbps, l.fec()), (25_000, Fec::Rs));
}
