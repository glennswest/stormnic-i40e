//! `EFI_DRIVER_BINDING_PROTOCOL`: which controllers this driver takes.
//! The same glue as stormnic-igb's, for the Intel 700 series.
//!
//! Supported looks at every PCI function the firmware offers. It reads the
//! vendor/device ID with a non-exclusive (GET_PROTOCOL) open and, for one of
//! ours, tries the BY_DRIVER open that Start will hold. A platform driver
//! that already owns the NIC (Intel's UNDI in the board firmware) holds that
//! open, so ours fails and the platform's driver wins.
//!
//! Start holds `EFI_PCI_IO_PROTOCOL` BY_DRIVER | EXCLUSIVE (`Held`), enables
//! memory decode and bus mastering (`decode`), identifies the PF
//! (`i40e::identify`), maps one DMA region, and brings the device up
//! (`i40e::bring_up`, each step traced): queues and interrupts a previous driver left are cleared,
//! the PF is reset, the admin queue is set up and the firmware asked for its
//! version, out of PXE mode, the MAC address, the VSI and its queue; the
//! HMC is laid out, the link is read (autonegotiation restarted if it is
//! down) and, when it is up, a DMA check runs on the queue pair, which is
//! then stopped again. Last it makes the SNP child handle (`snp`): no queue
//! DMAs until the SNP's user initializes it. A failure after the region is
//! mapped tears the device down (`Snp::teardown`'s steps) before the region
//! is freed; if that fails the region is kept and bus mastering turned off.
//!
//! Every Intel 700-series function Supported sees is logged (trace).

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::time::Duration;
use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams, ScopedProtocol};
use uefi::mem::memory_map::MemoryType;
use uefi::proto::loaded_image::LoadedImage;
use uefi::{Handle, Identify, Result, Status};
use uefi_raw::protocol::device_path::DevicePathProtocol;
use uefi_raw::protocol::driver::DriverBindingProtocol;

use crate::console;
use crate::decode::{self, Enabled};
use crate::i40e::adminq::AdminQ;
use crate::i40e::{self, Bus, Dma, Error, ResetStage, Step};
use crate::ids::{self, Nic};
use crate::pci_io::{Location, PciIo, Width};
use crate::snp;

/// The device's registers through a PciIo memory BAR.
pub struct Bars<'a> {
    pub pci: &'a PciIo,
    pub bar: u8,
}

impl Bus for Bars<'_> {
    type Error = Status;
    fn r32(&mut self, offset: u32) -> core::result::Result<u32, Status> {
        self.pci.mem_read(Width::U32, self.bar, offset).map_err(|e| e.status())
    }
    fn w32(&mut self, offset: u32, value: u32) -> core::result::Result<(), Status> {
        self.pci.mem_write(Width::U32, self.bar, offset, value).map_err(|e| e.status())
    }
    fn delay_us(&mut self, micros: usize) {
        boot::stall(Duration::from_micros(micros as u64));
    }
}

/// The register BAR: BAR 0 (64-bit memory) on every 700-series part.
const BAR: u8 = 0;

/// How long Start waits for the link after a restart of autonegotiation.
const LINK_WAIT_MS: usize = 4000;

/// A controller this driver has started.
struct Bound {
    controller: Handle,
    location: Option<Location>,
    /// How Start enabled memory decode and bus mastering; Stop undoes it.
    decode: Enabled<Status>,
    /// The admin queue, HMC pages, rings and buffers, mapped for DMA.
    dma: DmaRegion,
    /// The SNP, its child handle, the rings and the admin queue (`snp::create`).
    port: *mut snp::Port,
    /// The BY_DRIVER | EXCLUSIVE open; dropping it closes the protocol.
    pci: Held,
}

/// `i40e::DMA_PAGES` from AllocateBuffer, mapped as one common buffer.
struct DmaRegion {
    host: *mut u8,
    device: u64,
    mapping: *mut c_void,
}

impl DmaRegion {
    fn new(pci: &PciIo) -> Result<Self> {
        let host = pci.allocate_buffer(i40e::DMA_PAGES)?;
        // SAFETY: `host` is DMA_PAGES pages from AllocateBuffer.
        match unsafe { pci.map_common(host, i40e::DMA_PAGES * 4096) } {
            Ok((device, mapping)) => Ok(DmaRegion { host, device, mapping }),
            Err(e) => {
                // SAFETY: allocated above, never mapped or used.
                let _ = unsafe { pci.free_buffer(i40e::DMA_PAGES, host) };
                Err(e)
            }
        }
    }

    fn dma(&self) -> Dma { Dma { host: self.host, device: self.device } }

    /// Unmap and free. Only once the device is torn down.
    fn release(self, pci: &PciIo) {
        // SAFETY: the caller tore the device down (queues, admin queue and
        // HMC off, PF reset), so it no longer reads or writes the region;
        // nothing else holds the pointer after this.
        unsafe {
            if let Err(e) = pci.unmap(self.mapping) {
                say!("stormnic-i40e: could not unmap the DMA region: {:?}", e.status());
            }
            if let Err(e) = pci.free_buffer(i40e::DMA_PAGES, self.host) {
                say!("stormnic-i40e: could not free the DMA region: {:?}", e.status());
            }
        }
    }
}

#[derive(Default)]
pub struct I40eDriver {
    bound: Vec<Bound>,
}

impl I40eDriver {
    pub const fn new() -> Self {
        I40eDriver { bound: Vec::new() }
    }
}

/// What config space says about a function.
struct Ident {
    vendor: u16,
    device: u16,
    class: u8,
}

fn ident(pci: &PciIo) -> Result<Ident> {
    let id = pci.config_read_u32(0x00)?;
    let class = pci.config_read_u32(0x08)?;
    Ok(Ident { vendor: id as u16, device: (id >> 16) as u16, class: (class >> 24) as u8 })
}

/// `EFI_OPEN_PROTOCOL_BY_DRIVER | EFI_OPEN_PROTOCOL_EXCLUSIVE`.
const BY_DRIVER_EXCLUSIVE: u32 = 0x30;

/// Start's PciIo open, BY_DRIVER | EXCLUSIVE, closed on drop.
///
/// EXCLUSIVE because ConnectController goes on trying the other drivers
/// after ours has started, and one that opens PciIo EXCLUSIVE itself would
/// otherwise force ours off the function (stormnic-igb, from
/// stormnic-virtio#1). It never displaces a driver already there: Supported
/// declines any function held BY_DRIVER.
pub struct Held {
    iface: *const PciIo,
    agent: Handle,
    controller: Handle,
}

impl core::ops::Deref for Held {
    type Target = PciIo;
    fn deref(&self) -> &PciIo {
        // SAFETY: the open interface, valid until `drop` closes it (the
        // firmware calls Stop before it would remove it).
        unsafe { &*self.iface }
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        // SAFETY: closes the open made in `hold`.
        let _ = unsafe {
            (crate::snp::bs().close_protocol)(self.controller.as_ptr(), &PciIo::GUID, self.agent.as_ptr(), self.controller.as_ptr())
        };
    }
}

fn hold(agent: Handle, controller: Handle) -> Result<Held> {
    let mut iface: *mut c_void = core::ptr::null_mut();
    // SAFETY: an open tracked by the firmware, closed by Held's drop.
    let st = unsafe {
        (crate::snp::bs().open_protocol)(controller.as_ptr(), &PciIo::GUID, &mut iface, agent.as_ptr(),
            controller.as_ptr(), BY_DRIVER_EXCLUSIVE)
    };
    if st.is_error() || iface.is_null() {
        return Err(if st.is_error() { st } else { Status::DEVICE_ERROR }.into());
    }
    Ok(Held { iface: iface as *const PciIo, agent, controller })
}

fn open(agent: Handle, controller: Handle, attrs: OpenProtocolAttributes) -> Result<ScopedProtocol<PciIo>> {
    let params = OpenProtocolParams { handle: controller, agent, controller: Some(controller) };
    // SAFETY: GET_PROTOCOL opens are dropped before Supported/Start return;
    // a BY_DRIVER open is tracked by the firmware, which calls Stop before
    // it removes the interface.
    unsafe { boot::open_protocol::<PciIo>(params, attrs) }
}

pub fn at(location: Option<Location>) -> impl core::fmt::Display {
    struct At(Option<Location>);
    impl core::fmt::Display for At {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            match self.0 {
                Some(l) => write!(f, "{l}"),
                None => f.write_str("(location unknown)"),
            }
        }
    }
    At(location)
}

pub fn mac_str(m: [u8; 6]) -> impl core::fmt::Display {
    struct M([u8; 6]);
    impl core::fmt::Display for M {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            let m = self.0;
            write!(f, "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
        }
    }
    M(m)
}

/// The admin-queue opcode's name, for the console.
fn command(opcode: u16) -> &'static str {
    use crate::i40e::adminq::*;
    match opcode {
        OP_GET_VERSION => "Get Version",
        OP_QUEUE_SHUTDOWN => "Queue Shutdown",
        OP_MAC_ADDRESS_READ => "MAC Address Read",
        OP_CLEAR_PXE => "Clear PXE Mode",
        OP_GET_SWITCH_CONFIG => "Get Switch Config",
        OP_GET_VSI_PARAMS => "Get VSI Parameters",
        OP_ADD_MACVLAN => "Add MAC/VLAN",
        OP_SET_VSI_PROMISC => "Set VSI Promiscuous Modes",
        OP_RESTART_AN => "Restart AN",
        OP_GET_LINK_STATUS => "Get Link Status",
        _ => "command",
    }
}

pub fn error(e: &Error<Status>) -> impl core::fmt::Display + '_ {
    struct D<'a>(&'a Error<Status>);
    impl core::fmt::Display for D<'_> {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            match *self.0 {
                Error::Io(s) => write!(f, "register access failed: {s:?}"),
                Error::Removed => f.write_str("registers read all ones (function gone or BAR not decoded)"),
                Error::Reset { stage: ResetStage::Global, value } => write!(f, "a global reset did not finish (GLGEN_RSTAT {value:#010x})"),
                Error::Reset { stage: ResetStage::Firmware, value } => write!(f, "firmware not ready after reset (GLNVM_ULD {value:#010x})"),
                Error::Reset { stage: ResetStage::Pf, value } => write!(f, "PF reset did not complete (PFGEN_CTRL {value:#010x})"),
                Error::NoQueues { qalloc } => write!(f, "no LAN queues assigned to this PF (PFLAN_QALLOC {qalloc:#010x})"),
                Error::AdminSetup { bal } => write!(f, "admin queue base did not read back ({bal:#010x})"),
                Error::AdminTimeout { opcode, head } => write!(
                    f, "firmware did not answer {} ({opcode:#06x}) within 250 ms (ATQH {head})", command(opcode)
                ),
                Error::Admin { opcode, retval } => write!(f, "firmware refused {} ({opcode:#06x}): error {retval}", command(opcode)),
                Error::AdminApi { major, minor } => write!(f, "firmware admin API {major}.{minor}, this driver speaks 1.x"),
                Error::NoMac => f.write_str("firmware reports no valid LAN MAC address"),
                Error::NoVsi => f.write_str("switch configuration has no VSI"),
                Error::QueueRange { queue } => write!(f, "VSI queue {queue} is beyond the one HMC page this driver maps"),
                Error::Hmc { qmax, tx_size, rx_size } => write!(
                    f, "HMC cannot hold the queue contexts (LANQMAX {qmax}, objects {tx_size}/{rx_size} B)"
                ),
                Error::Unaligned { device } => write!(f, "DMA region at device {device:#x} is not page aligned"),
                Error::QueueTimeout { tx, value } => write!(
                    f, "{} queue did not follow its enable request in 100 ms (Q{}X_ENA {value:#010x})",
                    if tx { "transmit" } else { "receive" }, if tx { 'T' } else { 'R' }
                ),
                Error::Frame { len } => write!(f, "bad frame length {len}"),
            }
        }
    }
    D(e)
}

impl I40eDriver {
    fn is_bound(&self, controller: Handle) -> bool {
        self.bound.iter().any(|b| b.controller == controller)
    }

    /// The controller's entry, if it is one of ours.
    fn ours(&self, agent: Handle, controller: Handle) -> Result<(&'static Nic, Option<Location>)> {
        // No PciIo: not a PCI function; the common case, and silent.
        let pci = open(agent, controller, OpenProtocolAttributes::GetProtocol)?;
        let id = ident(&pci)?;
        if id.vendor != ids::INTEL || id.class != ids::CLASS_NETWORK {
            return Err(Status::UNSUPPORTED.into());
        }
        let location = pci.location().ok();
        let Some(nic) = ids::lookup(id.vendor, id.device) else {
            trace!("stormnic-i40e: {} 8086:{:04x}: Intel network function, not one this driver binds", at(location), id.device);
            return Err(Status::UNSUPPORTED.into());
        };
        Ok((nic, location))
    }

    fn supported(&mut self, agent: Handle, controller: Handle) -> Result {
        if self.is_bound(controller) {
            return Err(Status::ALREADY_STARTED.into());
        }
        let (nic, location) = self.ours(agent, controller)?;
        match open(agent, controller, OpenProtocolAttributes::ByDriver) {
            Ok(_pci) => {
                trace!("stormnic-i40e: {} 8086:{:04x} {}: Supported", at(location), nic.device, nic.name);
                Ok(())
            }
            Err(e) => {
                trace!(
                    "stormnic-i40e: {} 8086:{:04x} {}: already driven by another driver ({:?}); leaving it",
                    at(location), nic.device, nic.name, e.status()
                );
                Err(e)
            }
        }
    }

    fn start(&mut self, agent: Handle, controller: Handle) -> Result {
        console::begin();
        let (nic, location) = self.ours(agent, controller)?;
        let (at, dev) = (at(location), nic.device);
        let pci = match hold(agent, controller) {
            Ok(pci) => pci,
            Err(e) => {
                fail!("stormnic-i40e: {at} 8086:{dev:04x}: Start could not open PciIo BY_DRIVER | EXCLUSIVE: {:?}", e.status());
                return Err(e);
            }
        };
        let attributes = match decode::enable(&*pci) {
            Ok(e) => e,
            Err(decode::Error::Config(status)) => {
                fail!("stormnic-i40e: {at} 8086:{dev:04x}: Start could not enable memory decode and bus mastering: command register access failed: {status:?}");
                return Err(Status::UNSUPPORTED.into());
            }
            Err(decode::Error::NotSet(command, _)) => {
                fail!("stormnic-i40e: {at} 8086:{dev:04x}: Start could not enable memory decode and bus mastering: command {command:#06x}");
                return Err(Status::UNSUPPORTED.into());
            }
        };
        let mut bars = Bars { pci: &pci, bar: BAR };
        let id = match i40e::identify(&mut bars) {
            Ok(id) => id,
            Err(e) => {
                fail!("stormnic-i40e: {at} 8086:{dev:04x} {}: {}; releasing", nic.name, error(&e));
                restore(&pci, &attributes);
                return Err(Status::UNSUPPORTED.into());
            }
        };
        trace!(
            "stormnic-i40e: {at} 8086:{dev:04x} {}: PF {}, port {}, LAN queues {}..{}",
            nic.name, id.pf, id.port, id.base_queue, id.base_queue + id.queues - 1
        );
        let region = match DmaRegion::new(&pci) {
            Ok(r) => r,
            Err(e) => {
                fail!("stormnic-i40e: {at} 8086:{dev:04x}: DMA region not mapped: {:?}; releasing", e.status());
                restore(&pci, &attributes);
                return Err(Status::DEVICE_ERROR.into());
            }
        };
        let mut aq = AdminQ::new(region.dma());
        let up = match i40e::bring_up(&mut bars, &mut aq, region.dma(), &id, LINK_WAIT_MS, &mut |s| log_step(&at, dev, s)) {
            Ok(up) => up,
            Err(e) => {
                fail!("stormnic-i40e: {at} 8086:{dev:04x} {}: bring-up failed: {}", nic.name, error(&e));
                match i40e::release(&mut bars, &mut aq) {
                    Ok(()) => {
                        region.release(&pci);
                        say!("stormnic-i40e: {at} 8086:{dev:04x}: PF reset, DMA region released; releasing");
                    }
                    Err(e) => keep_dma(&pci, &at, &e),
                }
                restore(&pci, &attributes);
                return Err(Status::DEVICE_ERROR.into());
            }
        };
        let (mac, link, fw) = (up.net.mac, up.link, up.version);
        let port = match snp::create(agent.as_ptr(), controller.as_ptr(), &pci, BAR, up.net, aq, link, location) {
            Ok(p) => p,
            Err((status, in_use)) => {
                fail!("stormnic-i40e: {at} 8086:{dev:04x}: SNP not installed: {status:?}; releasing");
                if in_use {
                    keep_dma(&pci, &at, &Error::Io(status));
                } else {
                    region.release(&pci);
                }
                restore(&pci, &attributes);
                return Err(Status::DEVICE_ERROR.into());
            }
        };
        self.bound.push(Bound { controller, location, decode: attributes, dma: region, port, pci });
        // The one default line per NIC (stormnic-ixgbe#22).
        say!(
            "stormnic-i40e {}: {at} 8086:{dev:04x} {}: firmware {}.{} API {}.{}, MAC {}, link {}, SNP installed (verified in simulation only)",
            env!("CARGO_PKG_VERSION"), nic.name, fw.fw_major, fw.fw_minor, fw.api_major, fw.api_minor, mac_str(mac), link
        );
        console::begin();
        Ok(())
    }

    /// With `children`, remove the SNP child; without, release the NIC.
    fn stop(&mut self, agent: Handle, controller: Handle, children: &[uefi_raw::Handle]) -> Result {
        let Some(i) = self.bound.iter().position(|b| b.controller == controller) else {
            say!("stormnic-i40e: Stop for a controller this driver never started");
            return Err(Status::DEVICE_ERROR.into());
        };
        let (port, location) = (self.bound[i].port, self.bound[i].location);
        // SAFETY: `port` is live until `snp::destroy` below.
        let child = unsafe { (*port).child };
        if !children.is_empty() || child.is_some() {
            if child.is_some_and(|c| children.is_empty() || children.contains(&c)) {
                // SAFETY: as above.
                if let Err(s) = unsafe { snp::remove_child(agent.as_ptr(), controller.as_ptr(), port) } {
                    say!("stormnic-i40e: {}: Stop: SNP child still in use ({s:?}); kept", at(location));
                    return Err(Status::DEVICE_ERROR.into());
                }
                trace!("stormnic-i40e: {}: Stop: SNP child removed", at(location));
            }
            if !children.is_empty() {
                return Ok(());
            }
        }
        let b = self.bound.swap_remove(i);
        // SAFETY: the child is gone, so nothing else reaches the Port.
        match unsafe { snp::destroy(b.port) } {
            Ok(()) => b.dma.release(&b.pci),
            Err(e) => keep_dma(&b.pci, &at(b.location), &e),
        }
        restore(&b.pci, &b.decode);
        trace!("stormnic-i40e: {}: Stop: released", at(b.location));
        Ok(())
    }
}

/// The trace line for each bring-up step.
fn log_step(at: &impl core::fmt::Display, dev: u16, step: Step<Status>) {
    match step {
        Step::Reset(r) if r.global => trace!(
            "stormnic-i40e: {at} 8086:{dev:04x}: a global reset was in progress (PF reset not needed); firmware ready after {} ms",
            r.firmware_ms
        ),
        Step::Reset(r) => trace!(
            "stormnic-i40e: {at} 8086:{dev:04x}: PF reset in {} ms, firmware ready after {} ms", r.pf_ms, r.firmware_ms
        ),
        Step::Version(v) => trace!(
            "stormnic-i40e: {at} 8086:{dev:04x}: firmware {}.{} build {:#x}, API {}.{}, ROM {:#x}",
            v.fw_major, v.fw_minor, v.fw_build, v.api_major, v.api_minor, v.rom
        ),
        Step::Pxe(cleared) => trace!(
            "stormnic-i40e: {at} 8086:{dev:04x}: PXE mode {}", if cleared { "cleared" } else { "already clear" }
        ),
        Step::Vsi { mac, vsi } => trace!(
            "stormnic-i40e: {at} 8086:{dev:04x}: MAC {}, VSI {:#x}, queue {}, queue set {:#x}",
            mac_str(mac), vsi.seid, vsi.queue, vsi.qset
        ),
        Step::MacFilter(Ok(added)) => trace!(
            "stormnic-i40e: {at} 8086:{dev:04x}: MAC filter (any VLAN) {}", if added { "added" } else { "already there" }
        ),
        Step::MacFilter(Err(e)) => trace!(
            "stormnic-i40e: {at} 8086:{dev:04x}: MAC filter (any VLAN) not added: {}; the firmware's own filter stays", error(&e)
        ),
        Step::Hmc(l) => trace!(
            "stormnic-i40e: {at} 8086:{dev:04x}: HMC: {} queue context(s), {} B TX / {} B RX objects, RX contexts at {:#x}; DMA {} pages",
            l.count, l.tx_size, l.rx_size, l.rx_base, i40e::DMA_PAGES
        ),
        Step::Link { link, waited_ms: None } => trace!(
            "stormnic-i40e: {at} 8086:{dev:04x}: link {link} (PHY type {:#04x}, link info {:#04x}, AN info {:#04x})",
            link.phy_type, link.info, link.an
        ),
        Step::Link { link, waited_ms: Some(ms) } => trace!(
            "stormnic-i40e: {at} 8086:{dev:04x}: link was down; autonegotiation restarted, link {link} after {ms} ms (PHY type {:#04x}, link info {:#04x}, AN info {:#04x})",
            link.phy_type, link.info, link.an
        ),
        Step::Check(Some(ms)) => trace!(
            "stormnic-i40e: {at} 8086:{dev:04x}: DMA check: broadcast frame sent, descriptor done after {ms} ms"
        ),
        Step::Check(None) => say!(
            "stormnic-i40e: {at} 8086:{dev:04x}: DMA check: the device did not mark the frame's descriptor done within 100 ms"
        ),
    }
}

/// The device would not tear down: it might still DMA into the region, so
/// it is never freed. Bus mastering is turned off at the PCI level instead;
/// the pages stay allocated until reboot.
fn keep_dma(pci: &PciIo, at: &impl core::fmt::Display, e: &Error<Status>) {
    let off = decode::stop_bus_master(pci);
    fail!(
        "stormnic-i40e: {at}: could not tear the device down ({}); bus mastering {}, DMA region kept allocated",
        error(e),
        if off.is_ok() { "disabled" } else { "could not be disabled" }
    );
}

fn restore(pci: &PciIo, e: &Enabled<Status>) {
    if let Err(s) = decode::release(pci, e) {
        say!("stormnic-i40e: could not undo the PCI decode and bus-master changes: {s:?}");
    }
}

/// The driver binding interface and the driver behind it. `protocol` first:
/// firmware's `This` is the Binding.
#[repr(C)]
struct Binding {
    protocol: DriverBindingProtocol,
    driver: I40eDriver,
}

/// # Safety
/// `this` is the `protocol` of the leaked Binding; the firmware serializes
/// binding calls.
unsafe fn binding<'a>(this: *const DriverBindingProtocol) -> &'a mut Binding {
    unsafe { &mut *(this as *mut Binding) }
}

fn handles(this: *const DriverBindingProtocol, controller: uefi_raw::Handle) -> Option<(Handle, Handle)> {
    if this.is_null() { return None; }
    // SAFETY: the firmware passes our interface and a controller handle.
    unsafe {
        let agent = Handle::from_ptr((*this).driver_binding_handle)?;
        Some((agent, Handle::from_ptr(controller)?))
    }
}

unsafe extern "efiapi" fn binding_supported(this: *const DriverBindingProtocol, controller: uefi_raw::Handle,
    _remaining: *const DevicePathProtocol) -> Status {
    let Some((agent, controller)) = handles(this, controller) else { return Status::INVALID_PARAMETER };
    match unsafe { binding(this) }.driver.supported(agent, controller) {
        Ok(()) => Status::SUCCESS,
        Err(e) => e.status(),
    }
}

unsafe extern "efiapi" fn binding_start(this: *const DriverBindingProtocol, controller: uefi_raw::Handle,
    _remaining: *const DevicePathProtocol) -> Status {
    let Some((agent, controller)) = handles(this, controller) else { return Status::INVALID_PARAMETER };
    match unsafe { binding(this) }.driver.start(agent, controller) {
        Ok(()) => Status::SUCCESS,
        Err(e) => e.status(),
    }
}

unsafe extern "efiapi" fn binding_stop(this: *const DriverBindingProtocol, controller: uefi_raw::Handle,
    count: usize, children: *const uefi_raw::Handle) -> Status {
    let Some((agent, controller)) = handles(this, controller) else { return Status::INVALID_PARAMETER };
    if count > 0 && children.is_null() { return Status::INVALID_PARAMETER; }
    let children = if count == 0 { &[][..] } else {
        // SAFETY: the firmware passes `count` child handles.
        unsafe { core::slice::from_raw_parts(children, count) }
    };
    match unsafe { binding(this) }.driver.stop(agent, controller, children) {
        Ok(()) => Status::SUCCESS,
        Err(e) => e.status(),
    }
}

/// Install `EFI_DRIVER_BINDING_PROTOCOL` on the image handle. The image
/// must have been loaded as a boot-service driver (its code and data stay
/// after the entry point returns).
pub fn install() -> Result {
    let image = boot::image_handle();
    {
        let loaded = boot::open_protocol_exclusive::<LoadedImage>(image)?;
        if loaded.code_type() != MemoryType::BOOT_SERVICES_CODE || loaded.data_type() != MemoryType::BOOT_SERVICES_DATA {
            return Err(Status::UNSUPPORTED.into());
        }
    }
    let b = Box::into_raw(Box::new(Binding {
        protocol: DriverBindingProtocol {
            supported: binding_supported,
            start: binding_start,
            stop: binding_stop,
            version: 1,
            image_handle: image.as_ptr(),
            driver_binding_handle: image.as_ptr(),
        },
        driver: I40eDriver::new(),
    }));
    // SAFETY: the Binding is leaked, so the interface lives as long as the
    // image; the GUID matches the interface.
    let r = unsafe {
        boot::install_protocol_interface(Some(image), &DriverBindingProtocol::GUID, (&raw const (*b).protocol).cast())
    };
    if r.is_err() {
        // SAFETY: not installed, so nothing else holds it.
        drop(unsafe { Box::from_raw(b) });
    }
    r.map(|_| ())
}
