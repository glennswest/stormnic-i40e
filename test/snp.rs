//! The SNP state machine and data path against the simulated device in
//! `sim.rs`: the calls stormbootx's smoltcp (or a firmware MNP) makes, the
//! failure statuses from the UEFI specification, the switch filters behind
//! ReceiveFilters, link events, and the rule that the queues run only while
//! Initialized. Taken from stormnic-igb's, on the i40e simulator.
#[path = "../src/i40e.rs"]
#[allow(dead_code)]
mod i40e;
#[allow(dead_code)]
mod sim;
use i40e::adminq::{AdminQ, OP_GET_LINK_STATUS, OP_QUEUE_SHUTDOWN, OP_SET_VSI_PROMISC, PROMISC_RX_ONLY};
use i40e::snp::{self, Fail, Received, Snp, State};
use i40e::adminq::{PF_ARQLEN, PF_ATQLEN};
use i40e::{bring_up, identify, qrx_ena, qtx_ena, BUF_SIZE, QENA_STAT};
use sim::*;

const OTHER: [u8; 6] = [0x02, 9, 9, 9, 9, 9];
const MCAST: [u8; 6] = [0x01, 0x00, 0x5e, 0x00, 0x00, 0xfb];
const MCAST2: [u8; 6] = [0x33, 0x33, 0xff, 0x44, 0x55, 0x66];
type R<T> = Result<T, Fail<&'static str>>;

/// The device brought up as Start does, and its SNP (stopped).
fn snp() -> (Vec<u8>, Nic, Snp) {
    let (mem, mut nic, dma) = setup();
    let id = identify(&mut nic).unwrap();
    let mut aq = AdminQ::new(dma);
    let up = bring_up(&mut nic, &mut aq, dma, &id, 4000, &mut |_| {}).unwrap();
    nic.commands.clear();
    (mem, nic, Snp::new(up.net, aq, up.link))
}

/// Start, Initialize and the filters smoltcp asks for: unicast + broadcast.
fn up() -> (Vec<u8>, Nic, Snp) {
    let (mem, mut nic, mut s) = snp();
    s.start::<&str>().unwrap();
    s.initialize(&mut nic).unwrap();
    s.receive_filters(&mut nic, snp::UNICAST | snp::BROADCAST, 0, false, &[]).unwrap();
    (mem, nic, s)
}

fn running(nic: &Nic) -> bool {
    nic.reg(qrx_ena(0)) & QENA_STAT != 0 || nic.reg(qtx_ena(0)) & QENA_STAT != 0
}

/// A frame arrives and time passes (write-back on ITR flushes it).
fn arrive(nic: &mut Nic, f: &[u8]) {
    nic.deliver(f);
    i40e::Bus::delay_us(nic, 20);
}

fn rx(nic: &mut Nic, s: &mut Snp) -> R<Received> {
    let mut buf = [0u8; BUF_SIZE];
    s.receive(nic, &mut buf)
}

#[test]
fn calls_before_start_and_initialize_fail_as_the_spec_says() {
    let (_mem, mut nic, mut s) = snp();
    assert_eq!(s.initialize(&mut nic), Err(Fail::NotStarted));
    assert_eq!(rx(&mut nic, &mut s), Err(Fail::NotStarted));
    assert_eq!(s.stop(&mut nic), Err(Fail::NotStarted));
    s.start::<&str>().unwrap();
    assert_eq!(s.start::<&str>(), Err(Fail::AlreadyStarted));
    assert_eq!(rx(&mut nic, &mut s), Err(Fail::NotInitialized));
    assert_eq!(s.get_status(&mut nic, false), Err(Fail::NotInitialized));
    assert_eq!(s.reset(&mut nic), Err(Fail::NotInitialized));
    assert!(!running(&nic), "started is not running");
}

#[test]
fn initialize_runs_the_queues_and_turns_link_events_on() {
    let (_mem, mut nic, mut s) = up();
    assert_eq!(s.state, State::Initialized);
    assert!(running(&nic));
    assert!(s.media);
    assert!(nic.lse, "Get Link Status asked for events");
    assert!(nic.commands.contains(&OP_GET_LINK_STATUS));
    s.shutdown(&mut nic).unwrap();
    assert_eq!(s.state, State::Started);
    assert!(!running(&nic));
    s.initialize(&mut nic).unwrap();
    assert!(running(&nic));
    s.stop(&mut nic).unwrap();
    assert_eq!(s.state, State::Stopped);
    assert!(!running(&nic));
}

#[test]
fn a_failed_initialize_leaves_it_started_and_stopped() {
    let (_mem, mut nic, mut s) = snp();
    s.start::<&str>().unwrap();
    nic.stuck = Some(qtx_ena(0));
    assert!(matches!(s.initialize(&mut nic), Err(Fail::Device(_))));
    assert_eq!(s.state, State::Started);
    assert!(!running(&nic));
    // The firmware refusing the filter command stops the queues too.
    nic.stuck = None;
    nic.refuse = Some((OP_SET_VSI_PROMISC, 1));
    assert!(matches!(s.initialize(&mut nic), Err(Fail::Device(_))));
    assert_eq!(s.state, State::Started);
    assert!(!running(&nic));
}

#[test]
fn frames_are_filtered_by_the_setting() {
    let (_mem, mut nic, mut s) = up();
    arrive(&mut nic, &frame(OTHER, 1, 60));
    arrive(&mut nic, &frame(MCAST, 2, 60));
    arrive(&mut nic, &frame([0xff; 6], 3, 60));
    arrive(&mut nic, &frame(MAC, 4, 60));
    let r = rx(&mut nic, &mut s).unwrap();
    assert_eq!((r.destination, r.len, r.protocol), ([0xff; 6], 60, 0x0800));
    assert_eq!(rx(&mut nic, &mut s).unwrap().destination, MAC);
    assert_eq!(rx(&mut nic, &mut s), Err(Fail::NotReady));
    assert!(nic.dropped >= 2, "the switch dropped the other unicast and the multicast");
    // One multicast address: the switch passes every multicast, the SNP only it.
    s.receive_filters(&mut nic, snp::MULTICAST, 0, false, &[MCAST]).unwrap();
    assert_eq!(nic.promisc, (false, true, true));
    arrive(&mut nic, &frame(MCAST2, 5, 60));
    arrive(&mut nic, &frame(MCAST, 6, 60));
    assert_eq!(rx(&mut nic, &mut s).unwrap().destination, MCAST);
    assert_eq!(rx(&mut nic, &mut s), Err(Fail::NotReady));
    assert_eq!(s.receive_filters(&mut nic, 0, 0, false, &[OTHER]), Err(Fail::InvalidParameter));
    assert_eq!(s.receive_filters(&mut nic, 0x20, 0, false, &[]), Err(Fail::InvalidParameter));
}

#[test]
fn promiscuous_reaches_the_switch_receive_only() {
    let (_mem, mut nic, mut s) = up();
    s.receive_filters(&mut nic, snp::PROMISCUOUS, 0, false, &[]).unwrap();
    assert_eq!(nic.promisc, (true, true, true));
    let (flags, valid) = *nic.promisc_flags.last().unwrap();
    assert!(flags & PROMISC_RX_ONLY != 0 && valid & PROMISC_RX_ONLY != 0, "API 1.15: receive-only promiscuity");
    arrive(&mut nic, &frame(OTHER, 1, 60));
    assert_eq!(rx(&mut nic, &mut s).unwrap().destination, OTHER);
    s.receive_filters(&mut nic, 0, snp::PROMISCUOUS, false, &[]).unwrap();
    assert_eq!(nic.promisc, (false, false, true));
    // A Reset keeps the setting: the switch is told again.
    s.receive_filters(&mut nic, snp::PROMISCUOUS, 0, false, &[]).unwrap();
    nic.promisc = (false, false, false);
    s.reset(&mut nic).unwrap();
    assert_eq!(nic.promisc, (true, true, true));
    // No command when nothing changes.
    let n = nic.promisc_flags.len();
    s.receive_filters(&mut nic, snp::PROMISCUOUS, 0, false, &[]).unwrap();
    assert_eq!(nic.promisc_flags.len(), n);
}

#[test]
fn old_firmware_gets_no_receive_only_flag() {
    let (mem, mut nic, dma) = setup();
    nic.api = (1, 4);
    let id = identify(&mut nic).unwrap();
    let mut aq = AdminQ::new(dma);
    let up = bring_up(&mut nic, &mut aq, dma, &id, 4000, &mut |_| {}).unwrap();
    let mut s = Snp::new(up.net, aq, up.link);
    s.start::<&str>().unwrap();
    s.initialize(&mut nic).unwrap();
    s.receive_filters(&mut nic, snp::PROMISCUOUS, 0, false, &[]).unwrap();
    let (flags, valid) = *nic.promisc_flags.last().unwrap();
    assert_eq!((flags & PROMISC_RX_ONLY, valid & PROMISC_RX_ONLY), (0, 0));
    drop(mem);
}

#[test]
fn transmit_fills_the_header_and_get_status_hands_buffers_back_in_order() {
    let (_mem, mut nic, mut s) = up();
    let mut f = vec![0u8; 60];
    s.transmit(&mut nic, 14, &mut f, None, Some([0xff; 6]), Some(0x0806), 0x1000).unwrap();
    assert_eq!(&nic.wire[0][0..14], &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, MAC[0], MAC[1], MAC[2], MAC[3], MAC[4], MAC[5], 0x08, 0x06]);
    let mut g = frame(OTHER, 2, 60);
    s.transmit(&mut nic, 0, &mut g, None, None, None, 0x2000).unwrap();
    let (bits, t) = s.get_status(&mut nic, true).unwrap();
    assert_eq!((bits & snp::TRANSMIT_INTERRUPT, t), (snp::TRANSMIT_INTERRUPT, Some(0x1000)));
    assert_eq!(s.get_status(&mut nic, true).unwrap().1, Some(0x2000));
    assert_eq!(s.get_status(&mut nic, true).unwrap(), (0, None));
    assert_eq!(s.transmit(&mut nic, 14, &mut f, None, None, Some(1), 1), Err(Fail::InvalidParameter));
    assert_eq!(s.transmit(&mut nic, 0, &mut [0u8; 10], None, None, None, 1), Err(Fail::BufferTooSmall(14)));
    assert_eq!(s.transmit(&mut nic, 0, &mut [0u8; 1600], None, None, None, 1), Err(Fail::InvalidParameter));
}

#[test]
fn a_frame_bigger_than_the_buffer_reports_its_length_and_stays() {
    let (_mem, mut nic, mut s) = up();
    arrive(&mut nic, &frame(MAC, 1, 60));
    assert_eq!(s.receive(&mut nic, &mut [0u8; 30]), Err(Fail::BufferTooSmall(60)));
    assert_eq!(rx(&mut nic, &mut s).unwrap().len, 60);
}

#[test]
fn station_address_takes_only_the_firmware_s() {
    let (_mem, _nic, mut s) = up();
    assert_eq!(s.station_address::<()>(false, Some(MAC)), Ok(()));
    assert_eq!(s.station_address::<()>(false, Some(OTHER)), Err(Fail::Unsupported));
    assert_eq!(s.station_address::<()>(false, Some(MCAST)), Err(Fail::InvalidParameter));
    assert_eq!(s.station_address::<()>(true, None), Ok(()));
}

#[test]
fn wait_for_packet_sees_only_accepted_frames() {
    let (_mem, mut nic, mut s) = up();
    s.receive_filters(&mut nic, snp::PROMISCUOUS_MULTICAST, 0, false, &[]).unwrap();
    s.receive_filters(&mut nic, 0, snp::PROMISCUOUS_MULTICAST, false, &[]).unwrap();
    arrive(&mut nic, &frame(MCAST, 1, 60));
    assert!(!s.frame_waiting(&mut nic));
    arrive(&mut nic, &frame(MAC, 2, 60));
    assert!(s.frame_waiting(&mut nic));
    let (bits, _) = s.get_status(&mut nic, false).unwrap();
    assert_eq!(bits & snp::RECEIVE_INTERRUPT, snp::RECEIVE_INTERRUPT);
}

#[test]
fn get_status_follows_link_events() {
    let (_mem, mut nic, mut s) = up();
    assert!(s.media);
    assert!(nic.event(false));
    s.get_status(&mut nic, false).unwrap();
    assert!(!s.media);
    assert!(nic.event(true));
    s.get_status(&mut nic, false).unwrap();
    assert!(s.media);
    assert_eq!(s.link.mbps, 10_000);
    // No event: no command either, the state stays.
    s.get_status(&mut nic, false).unwrap();
    assert!(s.media);
    assert_eq!(nic.commands.iter().filter(|c| **c == OP_GET_LINK_STATUS).count(), 1, "only Initialize's");
}

#[test]
fn a_long_exchange_never_runs_out_of_buffers() {
    let (_mem, mut nic, mut s) = up();
    for i in 0..500u32 {
        arrive(&mut nic, &frame(MAC, i as u16, 60 + (i as usize % 1400)));
        assert_eq!(rx(&mut nic, &mut s).unwrap().len, 60 + (i as usize % 1400), "frame {i}");
        let mut f = frame(OTHER, i as u16, 60);
        s.transmit(&mut nic, 0, &mut f, None, None, None, i as usize).unwrap();
        i40e::Bus::delay_us(&mut nic, 20);
        assert_eq!(s.get_status(&mut nic, true).unwrap().1, Some(i as usize));
    }
    assert_eq!(nic.dropped, 0);
    assert_eq!(nic.wire.len(), 500);
}

#[test]
fn halt_stops_the_queues_in_every_state() {
    for state in [State::Stopped, State::Started, State::Initialized] {
        let (_mem, mut nic, mut s) = snp();
        if state != State::Stopped { s.start::<&str>().unwrap(); }
        if state == State::Initialized { s.initialize(&mut nic).unwrap(); }
        s.halt(&mut nic).unwrap();
        assert!(!running(&nic), "{state:?}");
        assert_eq!(s.state, State::Stopped);
    }
}

#[test]
fn teardown_leaves_nothing_that_dmas() {
    let (_mem, mut nic, mut s) = up();
    let resets = nic.pf_resets;
    s.teardown(&mut nic).unwrap();
    assert_eq!(s.state, State::Stopped);
    assert!(!running(&nic));
    assert!(nic.commands.contains(&OP_QUEUE_SHUTDOWN));
    assert_eq!(nic.reg(PF_ATQLEN), 0, "admin queue off");
    assert_eq!(nic.reg(PF_ARQLEN), 0);
    assert!(nic.sd.is_empty());
    assert_eq!(nic.pf_resets, resets + 1);
}

#[test]
fn teardown_fails_if_the_pf_will_not_reset() {
    let (_mem, mut nic, mut s) = up();
    nic.pfswr_polls = usize::MAX;
    assert!(s.teardown(&mut nic).is_err());
}

#[test]
fn mcast_ip_to_mac_maps_both_families() {
    let mut v4 = [0u8; 16];
    v4[..4].copy_from_slice(&[224, 0x80, 0, 251]);
    assert_eq!(snp::mcast_ip_to_mac(false, &v4), Some([0x01, 0x00, 0x5e, 0x00, 0x00, 251]));
    let mut v6 = [0u8; 16];
    v6[0] = 0xff;
    v6[12..].copy_from_slice(&[1, 2, 3, 4]);
    assert_eq!(snp::mcast_ip_to_mac(true, &v6), Some([0x33, 0x33, 1, 2, 3, 4]));
    assert_eq!(snp::mcast_ip_to_mac(false, &[10; 16]), None);
}
