//! stormnic-i40e: an `EFI_SIMPLE_NETWORK_PROTOCOL` driver for the Intel 700
//! series (X710, XL710, XXV710, X722), in Rust, for x86_64 and aarch64.
//!
//! An EFI boot-service driver (see build.rs). The entry point installs an
//! `EFI_DRIVER_BINDING_PROTOCOL` on the image handle (binding.rs) and returns;
//! the firmware's `ConnectController` then calls Supported/Start for each
//! controller. Start resets the PF, talks to the device firmware over the
//! admin queue (version, PXE mode off, MAC address, the VSI and its queue,
//! link), lays out the HMC for one queue pair, runs a DMA check when the
//! link is up, stops the queues and puts an `EFI_SIMPLE_NETWORK_PROTOCOL` on
//! a child handle; the user (stormbootx's smoltcp) opens it EXCLUSIVE, and
//! only its Initialize starts the queues.
//!
//! Verified against a simulated device only (no 700-series NIC in the lab):
//! the per-NIC console line says so.
#![no_main]
#![no_std]

extern crate alloc;

// First: its macros (say!, trace!, fail!, note!) are used by the modules below.
#[macro_use]
mod console;
mod binding;
mod decode;
mod i40e;
mod ids;
mod pci_io;
mod snp;
mod trace;

use uefi::prelude::*;

#[entry]
fn main() -> Status {
    if uefi::helpers::init().is_err() {
        return Status::LOAD_ERROR;
    }
    console::init();
    log::set_max_level(if console::verbose() { log::LevelFilter::Info } else { log::LevelFilter::Warn });
    let version = env!("CARGO_PKG_VERSION");
    match binding::install() {
        Ok(()) => {
            trace!(
                "stormnic-i40e {version}: driver binding installed ({} Intel 700-series device IDs)",
                ids::SUPPORTED.len()
            );
            Status::SUCCESS
        }
        Err(e) => {
            say!("stormnic-i40e {version}: driver binding not installed: {:?}", e.status());
            e.status()
        }
    }
}
