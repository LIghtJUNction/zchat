#![no_std]
//! A two-method, versioned native provider boundary for zchat.
//!
//! The plugin returns origin-owned bytes; zplugin's generated host proxy returns
//! a `Buffer<'a>` that keeps the library alive until those bytes are released.
//! Native providers run with the host process's permissions and must be trusted.

pub use zplugin;

pub const ABI_VERSION: u32 = 1;
pub const INTERFACE_ID: &str = "projectz.zchat.im";
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Native methods run on the creating thread. A provider may own workers but
/// must coordinate them internally and preserve backend event ordering.
///
/// `manifest` describes actual supported operations and backend requirements.
/// `invoke` accepts one bounded UTF-8 JSON request. Platform/application failures
/// are returned as structured JSON; zplugin errors signal an ABI-level failure.
/// Request and response ownership never crosses the Rust allocator boundary.
#[zplugin::interface(id = "projectz.zchat.im", version = 1)]
pub trait ImPlugin {
    fn manifest(&self) -> zplugin::Result<zplugin::OwnedBytes>;
    fn invoke(&mut self, request: &[u8]) -> zplugin::Result<zplugin::OwnedBytes>;
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    use super::*;
    use alloc::vec;

    #[test]
    fn freezes_the_two_method_v1_contract() {
        assert_eq!(ImPluginApi::SPEC.id, INTERFACE_ID);
        assert_eq!(ImPluginApi::SPEC.version, ABI_VERSION);
        assert_eq!(ImPluginApi::SPEC.signatures.len(), 2);
    }

    #[test]
    fn plugin_owned_bytes_preserve_exact_payload() {
        let bytes = zplugin::OwnedBytes::from_vec(vec![0, 1, 255]);
        assert_eq!(&*bytes, &[0, 1, 255]);
    }
}
