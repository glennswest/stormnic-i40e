//! Memory ordering between the CPU, descriptor memory and the NIC; taken
//! from stormnic-realtek (its #2: aarch64).
//!
//! The descriptor rings and buffers live in memory the NIC reads and writes
//! by DMA, and the NIC is told about new work by an MMIO write. On x86_64
//! stores are seen in program order and uncached MMIO is not reordered with
//! them, so a compiler fence is all that is needed. On aarch64 neither holds:
//! a store to a descriptor may still be in flight when the tail register
//! write reaches the device, and a load of a buffer may run ahead of the
//! load that saw DD set. The NIC is in the outer-shareable domain, so the
//! barriers are the `osh` forms:
//!
//! - `dma_wmb`: every earlier store to DMA memory is visible to the NIC
//!   before any later one (`dmb oshst`). After filling a descriptor.
//! - `dma_rmb`: every later load from DMA memory happens after the earlier
//!   one (`dmb oshld`). Between seeing DD set and reading the length and
//!   the frame.
//! - `mmio_wmb`: every earlier store to memory is complete before the next
//!   MMIO write (`dsb st`). Before a tail-register write, so the NIC never
//!   fetches a descriptor that is not there yet.
//!
//! Cache coherency is not handled here: the memory is PciIo AllocateBuffer +
//! Map(BusMasterCommonBuffer), which the firmware makes uncached on a
//! platform whose PCIe is not cache coherent (UEFI spec 14.4,
//! `EFI_PCI_IO_PROTOCOL.Map`).

#[inline(always)]
pub fn dma_wmb() {
    #[cfg(target_arch = "aarch64")]
    // SAFETY: a barrier; no memory or register is changed.
    unsafe { core::arch::asm!("dmb oshst", options(nostack, preserves_flags)) };
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
}

#[inline(always)]
pub fn dma_rmb() {
    #[cfg(target_arch = "aarch64")]
    // SAFETY: a barrier; no memory or register is changed.
    unsafe { core::arch::asm!("dmb oshld", options(nostack, preserves_flags)) };
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
}

#[inline(always)]
pub fn mmio_wmb() {
    #[cfg(target_arch = "aarch64")]
    // SAFETY: a barrier; no memory or register is changed.
    unsafe { core::arch::asm!("dsb st", options(nostack, preserves_flags)) };
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
}
