//! Link the image as an EFI boot-service driver, not an application.
//!
//! The PE32+ subsystem decides how the firmware loads the image (UEFI spec
//! 2.1.2): an application is unloaded when its entry point returns, a
//! boot-service driver stays resident in boot-services memory. The driver
//! binding installed by the entry point holds pointers into this image, so it
//! must stay. rustc's UEFI targets link `/SUBSYSTEM:EFI_APPLICATION` by
//! default; a later `/SUBSYSTEM` on the lld-link command line wins.
//!
//! `/Brepro` and `/DEBUG:NONE` (as stormnic-ixgbe#12): the PE time stamp is a
//! hash of the image and there is no CodeView record, so the image does not
//! change with the time or the build directory.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.ends_with("-uefi") {
        println!("cargo:rustc-link-arg-bin=stormnic-i40e=/SUBSYSTEM:EFI_BOOT_SERVICE_DRIVER");
        println!("cargo:rustc-link-arg-bin=stormnic-i40e=/Brepro");
        println!("cargo:rustc-link-arg-bin=stormnic-i40e=/DEBUG:NONE");
    }
}
