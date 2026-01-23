//! Zero-copy shared memory buffer type.
//!
//! `ShmBytes` provides zero-copy ownership transfer of shared memory buffers
//! between services. Instead of copying data through the transport layer,
//! services can pass handles to pre-allocated SHM slots.
//!
//! # Usage
//!
//! ```ignore
//! // Allocate a buffer
//! let mut buf = ShmBytes::alloc(1024)?;
//! buf.as_mut_slice().copy_from_slice(&data);
//!
//! // Pass to another service - ownership transfers, no copy
//! let result = other_service.process(buf).await?;
//!
//! // The receiving service can read the data directly from SHM
//! // When dropped (or explicitly freed), the slot returns to the pool
//! ```
//!
//! # Ownership Model
//!
//! - `ShmBytes` is move-only (no `Clone`)
//! - When passed through roam, only the handle crosses the wire
//! - The receiver becomes the owner
//! - On drop, the slot is returned to the pool (if in SHM context)
//! - Crash recovery reclaims slots from dead peers

use std::ops::Deref;
use std::sync::Arc;

use facet::Facet;
use tokio::task_local;

use crate::var_slot_pool::{VarFreeError, VarSlotHandle, VarSlotPool};

// ============================================================================
// Task-local SHM Pool Context
// ============================================================================

task_local! {
    /// The variable-size slot pool for the current SHM transport context.
    ///
    /// Set by the SHM transport before dispatching to service methods.
    /// Used by `ShmBytes` for allocation and freeing.
    pub static SHM_POOL: Arc<VarSlotPool>;

    /// The local peer ID (0 for host, 1-255 for guests).
    ///
    /// Set by the SHM transport before dispatching to service methods.
    /// Used for ownership tracking in `ShmBytes`.
    pub static SHM_LOCAL_PEER_ID: u8;
}

// ============================================================================
// Wire Format Type
// ============================================================================

/// Wire format for `ShmBytes` - includes both handle and length.
///
/// When serialized, `ShmBytes` is represented as this type on the wire.
/// This allows the receiver to reconstruct the full `ShmBytes` with the
/// correct length without needing to look it up in slot metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Facet)]
pub struct ShmBytesWire {
    /// The slot handle identifying the shared memory location.
    pub handle: VarSlotHandle,
    /// The actual data length (not the slot size).
    pub len: u32,
}

// ============================================================================
// Error Types
// ============================================================================

/// Errors from `ShmBytes` operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShmError {
    /// Not in an SHM transport context (task-local not set).
    NoContext,
    /// No slot available in the pool (backpressure).
    SlotExhausted,
    /// The requested size exceeds the maximum slot size.
    SizeTooLarge,
    /// Error freeing the slot.
    FreeError(VarFreeError),
}

impl std::fmt::Display for ShmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShmError::NoContext => write!(f, "not in SHM transport context"),
            ShmError::SlotExhausted => write!(f, "no SHM slots available"),
            ShmError::SizeTooLarge => write!(f, "requested size exceeds maximum slot size"),
            ShmError::FreeError(e) => write!(f, "failed to free SHM slot: {:?}", e),
        }
    }
}

impl std::error::Error for ShmError {}

impl From<VarFreeError> for ShmError {
    fn from(e: VarFreeError) -> Self {
        ShmError::FreeError(e)
    }
}

// ============================================================================
// ShmBytes - Zero-Copy Shared Memory Buffer
// ============================================================================

/// A zero-copy shared memory buffer.
///
/// Wraps a slot in a variable-size slot pool. The handle can be passed between
/// services without copying the underlying data - only the handle and length
/// cross the wire.
///
/// # Wire Format
///
/// Serializes as `ShmBytesWire` containing the slot handle and data length.
/// The actual bytes stay in shared memory.
///
/// # Ownership
///
/// - Move-only (does not implement `Clone`)
/// - On drop, returns the slot to the pool if in SHM context
/// - Crash recovery reclaims slots from dead peers
#[derive(Facet)]
#[facet(proxy = ShmBytesWire)]
pub struct ShmBytes {
    handle: VarSlotHandle,
    len: usize,
}

// Ensure ShmBytes is not Clone
static_assertions::assert_not_impl_any!(ShmBytes: Clone);

impl ShmBytes {
    /// Create an `ShmBytes` from a handle and length.
    ///
    /// This is called during deserialization (hydration) when receiving
    /// an `ShmBytes` from another service.
    pub(crate) fn from_handle(handle: VarSlotHandle, len: usize) -> Self {
        Self { handle, len }
    }

    /// Allocate a new `ShmBytes` buffer of the given size.
    ///
    /// Must be called within an SHM transport context.
    pub fn alloc(size: usize) -> Result<Self, ShmError> {
        // Get the local peer ID (0 if not set = host)
        let owner = SHM_LOCAL_PEER_ID.try_with(|&id| id).unwrap_or(0);
        
        tracing::debug!("ShmBytes::alloc: attempting to access SHM_POOL");
        SHM_POOL
            .try_with(|pool| {
                tracing::debug!("ShmBytes::alloc: SHM_POOL is available");
                let handle = pool
                    .alloc(size as u32, owner)
                    .ok_or(ShmError::SlotExhausted)?;
                Ok(Self {
                    handle,
                    len: size,
                })
            })
            .map_err(|_| {
                tracing::debug!("ShmBytes::alloc: SHM_POOL not available (NoContext)");
                ShmError::NoContext
            })?
    }

    /// Get the handle for this buffer.
    pub fn handle(&self) -> VarSlotHandle {
        self.handle
    }

    /// Get the length of the buffer in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Check if the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Get a slice of the buffer contents.
    ///
    /// Must be called within an SHM transport context.
    pub fn as_slice(&self) -> Option<&[u8]> {
        SHM_POOL
            .try_with(|pool| {
                pool.payload_ptr(self.handle).map(|ptr| {
                    // SAFETY: The pool ensures the pointer is valid for `len` bytes
                    // as long as we hold the handle and are in the SHM context.
                    unsafe { std::slice::from_raw_parts(ptr, self.len) }
                })
            })
            .ok()
            .flatten()
    }

    /// Get a mutable slice of the buffer contents.
    ///
    /// Must be called within an SHM transport context.
    pub fn as_mut_slice(&mut self) -> Option<&mut [u8]> {
        SHM_POOL
            .try_with(|pool| {
                pool.payload_ptr(self.handle).map(|ptr| {
                    // SAFETY: The pool ensures the pointer is valid for `len` bytes
                    // as long as we hold the handle and are in the SHM context.
                    // We have exclusive access via &mut self.
                    unsafe { std::slice::from_raw_parts_mut(ptr, self.len) }
                })
            })
            .ok()
            .flatten()
    }

    /// Explicitly free this buffer, returning the slot to the pool.
    ///
    /// Returns an error if not in an SHM transport context or if the free fails.
    /// If you don't need error handling, just drop the `ShmBytes` instead.
    pub fn free(self) -> Result<(), ShmError> {
        let result = SHM_POOL
            .try_with(|pool| pool.free_allocated(self.handle))
            .map_err(|_| ShmError::NoContext)?;

        // Prevent Drop from running (we handled it)
        std::mem::forget(self);

        result.map_err(ShmError::FreeError)
    }

    /// Mark this buffer as in-flight (being sent to another peer).
    ///
    /// This transitions the slot from `Allocated` to `InFlight` state.
    /// Called automatically during serialization when passing `ShmBytes` in RPC.
    /// The receiver will call `claim_in_flight` to take ownership.
    ///
    /// After calling this, you should NOT drop the `ShmBytes` - the receiver
    /// is now responsible for freeing it.
    pub fn mark_in_flight(&self) -> Result<(), ShmError> {
        tracing::debug!("ShmBytes::mark_in_flight: attempting to access SHM_POOL");
        SHM_POOL
            .try_with(|pool| {
                tracing::debug!("ShmBytes::mark_in_flight: SHM_POOL is available");
                pool.mark_in_flight(self.handle)
            })
            .map_err(|_| {
                tracing::debug!("ShmBytes::mark_in_flight: SHM_POOL not available (NoContext)");
                ShmError::NoContext
            })?
            .map_err(ShmError::FreeError)
    }

    /// Claim this buffer from in-flight state (received from another peer).
    ///
    /// This transitions the slot from `InFlight` to `Allocated` state
    /// and updates the owner to the local peer.
    pub(crate) fn claim_in_flight(&self) -> Result<(), ShmError> {
        let owner = SHM_LOCAL_PEER_ID.try_with(|&id| id).unwrap_or(0);
        
        SHM_POOL
            .try_with(|pool| pool.claim_in_flight(self.handle, owner))
            .map_err(|_| ShmError::NoContext)?
            .map_err(ShmError::FreeError)
    }
}

impl Drop for ShmBytes {
    fn drop(&mut self) {
        // Best-effort: free if we're in an SHM context.
        // If not, crash recovery will reclaim the slot when the peer dies.
        let _ = SHM_POOL.try_with(|pool| {
            let _ = pool.free_allocated(self.handle);
        });
    }
}

impl Deref for ShmBytes {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.as_slice().unwrap_or(&[])
    }
}

impl std::fmt::Debug for ShmBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShmBytes")
            .field("handle", &self.handle)
            .field("len", &self.len)
            .finish()
    }
}

// ============================================================================
// Facet Proxy Implementation
// ============================================================================

/// Serialization: `&ShmBytes` -> `ShmBytesWire`
///
/// This is a pure data conversion - the mark_in_flight transition is handled
/// separately by `mark_shm_bytes_in_flight()` which the transport calls before
/// serialization. This separation prevents side effects during debug printing
/// or other non-transport serialization.
impl TryFrom<&ShmBytes> for ShmBytesWire {
    type Error = std::convert::Infallible;

    fn try_from(bytes: &ShmBytes) -> Result<Self, Self::Error> {
        Ok(ShmBytesWire {
            handle: bytes.handle,
            len: bytes.len as u32,
        })
    }
}

/// Deserialization: `ShmBytesWire` -> `ShmBytes`
///
/// Creates an `ShmBytes` with the length from the wire format.
/// The `patch_shm_bytes` hook will claim ownership of the in-flight slot.
impl TryFrom<ShmBytesWire> for ShmBytes {
    type Error = std::convert::Infallible;

    fn try_from(wire: ShmBytesWire) -> Result<Self, Self::Error> {
        Ok(ShmBytes {
            handle: wire.handle,
            len: wire.len as usize,
        })
    }
}

// ============================================================================
// Mark In-Flight Support (Sender Side)
// ============================================================================

/// Mark all `ShmBytes` instances in a structure as in-flight before sending.
///
/// This walks the structure using facet reflection to find `ShmBytes` fields
/// and transitions them from `Allocated` to `InFlight` state. Call this
/// BEFORE serialization in the transport layer.
///
/// The separation of marking from proxy conversion prevents side effects when
/// debug-printing or otherwise serializing `ShmBytes` outside of transport code.
pub fn mark_shm_bytes_in_flight<T: Facet<'static>>(data: &T) {
    let _ = SHM_POOL.try_with(|pool| {
        let peek = facet::Peek::new(data);
        mark_shm_bytes_recursive(peek, pool);
    });
}

/// Hook form of `mark_shm_bytes_in_flight` for use with `MARK_IN_FLIGHT_HOOK`.
///
/// This is the function pointer form suitable for use with the dispatch hook mechanism.
pub fn mark_shm_bytes_in_flight_hook(peek: facet::Peek<'_, '_>) {
    let _ = SHM_POOL.try_with(|pool| {
        mark_shm_bytes_recursive(peek, pool);
    });
}

fn mark_shm_bytes_recursive(peek: facet::Peek<'_, '_>, pool: &VarSlotPool) {
    let shape = peek.shape();

    // Check if this is an ShmBytes type
    if shape.type_identifier == "ShmBytes" {
        if let Ok(ps) = peek.into_struct() {
            // Read the handle to mark it
            let handle_opt = ps
                .field_by_name("handle")
                .ok()
                .and_then(|f| f.get::<VarSlotHandle>().ok().cloned());

            if let Some(handle) = handle_opt {
                if let Err(e) = pool.mark_in_flight(handle) {
                    tracing::warn!("Failed to mark ShmBytes handle {:?} as in-flight: {:?}", handle, e);
                }
            }
        }
        return;
    }

    // Recurse into Option<T>
    if let Ok(po) = peek.into_option() {
        if let Some(inner) = po.value() {
            mark_shm_bytes_recursive(inner, pool);
        }
        return;
    }

    // Recurse into struct/tuple fields
    if let Ok(ps) = peek.into_struct() {
        let field_count = ps.field_count();
        for i in 0..field_count {
            if let Ok(field_peek) = ps.field(i) {
                mark_shm_bytes_recursive(field_peek, pool);
            }
        }
        return;
    }

    // Recurse into enum variants
    if let Ok(pe) = peek.into_enum() {
        if let Ok(Some(variant_peek)) = pe.field(0) {
            mark_shm_bytes_recursive(variant_peek, pool);
        }
        return;
    }

    // Recurse into sequences (e.g., Vec<ShmBytes>)
    if let Ok(pl) = peek.into_list() {
        for element in pl.iter() {
            mark_shm_bytes_recursive(element, pool);
        }
    }
}

// ============================================================================
// Hydration Support (Receiver Side)
// ============================================================================

/// Patch `ShmBytes` instances in deserialized data to claim ownership.
///
/// This walks the structure using facet reflection to find `ShmBytes` fields
/// and claims ownership of the in-flight slots. The length is already correct
/// from deserialization (via `ShmBytesWire`).
///
/// Call this after `facet_postcard::from_slice` but before passing args to handlers.
pub fn patch_shm_bytes<T: Facet<'static>>(data: &mut T) {
    let _ = SHM_POOL.try_with(|pool| {
        let poke = facet::Poke::new(data);
        patch_shm_bytes_recursive(poke, pool);
    });
}

/// Patch hook for `ShmBytes` - can be registered as a `PATCH_HOOK` in roam-session.
///
/// This is the function pointer form suitable for use with the dispatch hook mechanism.
/// It patches `ShmBytes` lengths using the `SHM_POOL` task-local.
pub fn patch_shm_bytes_hook(poke: facet::Poke<'_, '_>) {
    let _ = SHM_POOL.try_with(|pool| {
        patch_shm_bytes_recursive(poke, pool);
    });
}

fn patch_shm_bytes_recursive(mut poke: facet::Poke<'_, '_>, pool: &VarSlotPool) {
    use facet::Def;

    let shape = poke.shape();

    // Check if this is an ShmBytes type
    if shape.type_identifier == "ShmBytes" {
        // Get the handle to claim ownership of the in-flight slot
        if let Ok(mut ps) = poke.into_struct() {
            // Read the handle to get slot info
            let handle_opt = ps
                .field_by_name("handle")
                .ok()
                .and_then(|f| f.get::<VarSlotHandle>().ok().cloned());

            if let Some(handle) = handle_opt {
                // Claim ownership of this in-flight slot
                // Get the local peer ID (0 if not set = host)
                let owner = SHM_LOCAL_PEER_ID.try_with(|&id| id).unwrap_or(0);
                if let Err(e) = pool.claim_in_flight(handle, owner) {
                    // Log but continue - this might fail if the slot was already
                    // claimed or if it's in an unexpected state
                    tracing::warn!("Failed to claim ShmBytes handle {:?}: {:?}", handle, e);
                }
                // Length is already set from wire format (ShmBytesWire), no need to patch it
            }
        }
        return;
    }

    // Dispatch based on shape's definition
    match shape.def {
        Def::Scalar => {}

        Def::Option(option_def) => {
            // For Option<T>, use the OptionVTable to check if it's Some and get the inner value
            let data_ptr = poke.data_mut();
            let ptr_const = facet::PtrConst::new(data_ptr.as_byte_ptr());
            let is_some = unsafe { (option_def.vtable.is_some)(ptr_const) };
            
            if is_some {
                // Get pointer to inner value
                let inner_ptr = unsafe { (option_def.vtable.get_value)(ptr_const) };
                if let Some(inner_ptr) = inner_ptr {
                    // Create a Poke for the inner value
                    // SAFETY: We have exclusive access via the outer Poke, and the pointer is valid
                    let inner_poke = unsafe {
                        facet::Poke::from_raw_parts(
                            facet::PtrMut::new(inner_ptr.as_byte_ptr() as *mut u8),
                            option_def.t,
                        )
                    };
                    patch_shm_bytes_recursive(inner_poke, pool);
                }
            }
        }

        Def::List(list_def) => {
            // Get the container's shape (e.g., Vec<ShmBytes>) - needed for get_mut vtable
            let container_shape = poke.shape();
            let len = {
                let peek = poke.as_peek();
                peek.into_list().map(|pl| pl.len()).unwrap_or(0)
            };
            if let Some(get_mut_fn) = list_def.vtable.get_mut {
                let element_shape = list_def.t;
                let data_ptr = poke.data_mut();
                for i in 0..len {
                    // SAFETY: Exclusive mutable access via poke, index < len
                    // Note: Pass container_shape (Vec<T>), not element_shape (T)
                    // The vtable uses shape.type_params[0] to get element size
                    let element_ptr = unsafe { (get_mut_fn)(data_ptr, i, container_shape) };
                    if let Some(ptr) = element_ptr {
                        let element_poke = unsafe { facet::Poke::from_raw_parts(ptr, element_shape) };
                        patch_shm_bytes_recursive(element_poke, pool);
                    }
                }
            }
        }

        _ if poke.is_struct() => {
            let mut ps = poke.into_struct().expect("is_struct was true");
            let field_count = ps.field_count();
            for i in 0..field_count {
                if let Ok(field_poke) = ps.field(i) {
                    patch_shm_bytes_recursive(field_poke, pool);
                }
            }
        }

        _ if poke.is_enum() => {
            if let Ok(mut pe) = poke.into_enum() {
                if let Ok(Some(variant_poke)) = pe.field(0) {
                    patch_shm_bytes_recursive(variant_poke, pool);
                }
            }
        }

        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::SizeClass;
    use crate::var_slot_pool::VarSlotPool;
    use shm_primitives::HeapRegion;

    fn create_test_pool() -> (HeapRegion, Arc<VarSlotPool>) {
        let classes = vec![
            SizeClass::new(64, 16),  // Small: 64 bytes × 16 slots
            SizeClass::new(256, 8),  // Medium: 256 bytes × 8 slots
            SizeClass::new(1024, 4), // Large: 1 KB × 4 slots
        ];

        let size = VarSlotPool::calculate_size(&classes);
        let region = HeapRegion::new_zeroed(size as usize);
        let mut pool = VarSlotPool::new(region.region(), 0, classes);

        unsafe { pool.init() };

        (region, Arc::new(pool))
    }

    #[test]
    fn shm_bytes_is_not_clone() {
        // This is a compile-time check via static_assertions above
    }

    #[test]
    fn test_var_slot_handle_facet_roundtrip() {
        // Test that VarSlotHandle can be serialized and deserialized via facet
        let handle = VarSlotHandle {
            class_idx: 2,
            extent_idx: 1,
            slot_idx: 12345,
            generation: 42,
        };

        // Serialize
        let bytes = facet_postcard::to_vec(&handle).expect("serialize handle");

        // Deserialize
        let decoded: VarSlotHandle =
            facet_postcard::from_slice(&bytes).expect("deserialize handle");

        assert_eq!(decoded.class_idx, handle.class_idx);
        assert_eq!(decoded.extent_idx, handle.extent_idx);
        assert_eq!(decoded.slot_idx, handle.slot_idx);
        assert_eq!(decoded.generation, handle.generation);
    }

    #[test]
    fn test_shm_bytes_proxy_serialization() {
        // Test that ShmBytes serializes as VarSlotHandle (the proxy)
        let (_region, pool) = create_test_pool();

        // Run in task-local context
        SHM_POOL.sync_scope(Arc::clone(&pool), || {
            let bytes = ShmBytes::alloc(32).expect("alloc");
            let handle = bytes.handle();

            // Serialize the ShmBytes - should produce VarSlotHandle bytes
            let serialized = facet_postcard::to_vec(&bytes).expect("serialize ShmBytes");

            // Deserialize as VarSlotHandle to verify proxy behavior
            let decoded_handle: VarSlotHandle =
                facet_postcard::from_slice(&serialized).expect("deserialize as handle");

            assert_eq!(decoded_handle.class_idx, handle.class_idx);
            assert_eq!(decoded_handle.extent_idx, handle.extent_idx);
            assert_eq!(decoded_handle.slot_idx, handle.slot_idx);
            assert_eq!(decoded_handle.generation, handle.generation);
        });
    }

    #[test]
    fn test_mark_shm_bytes_in_flight() {
        use shm_primitives::SlotState;
        
        // Test that mark_shm_bytes_in_flight properly marks slots before serialization
        let (_region, pool) = create_test_pool();

        SHM_POOL.sync_scope(Arc::clone(&pool), || {
            let bytes = ShmBytes::alloc(32).expect("alloc");
            let handle = bytes.handle();

            // Verify initial state is Allocated
            let meta = pool
                .slot_meta_ext(handle.class_idx as usize, handle.extent_idx as usize, handle.slot_idx)
                .expect("meta");
            assert_eq!(meta.state(), SlotState::Allocated);

            // Mark in-flight explicitly (what dispatch_call does before serialization)
            mark_shm_bytes_in_flight(&bytes);

            // After marking, state should be InFlight
            assert_eq!(meta.state(), SlotState::InFlight);

            // Serialization should NOT mark again (it's a pure data conversion now)
            let _serialized = facet_postcard::to_vec(&bytes).expect("serialize");
            assert_eq!(meta.state(), SlotState::InFlight); // Still InFlight, not double-marked

            // Now forget the bytes (ownership transferred to receiver)
            std::mem::forget(bytes);

            // Clean up via pool.free() (which expects InFlight state)
            pool.free(handle).expect("free");
        });
    }

    #[test]
    fn test_mark_shm_bytes_in_struct() {
        use shm_primitives::SlotState;
        
        #[derive(Facet)]
        struct Container {
            data: ShmBytes,
            name: String,
        }

        let (_region, pool) = create_test_pool();

        SHM_POOL.sync_scope(Arc::clone(&pool), || {
            let bytes = ShmBytes::alloc(64).expect("alloc");
            let handle = bytes.handle();
            
            let container = Container {
                data: bytes,
                name: "test".to_string(),
            };

            // Verify initial state
            let meta = pool
                .slot_meta_ext(handle.class_idx as usize, handle.extent_idx as usize, handle.slot_idx)
                .expect("meta");
            assert_eq!(meta.state(), SlotState::Allocated);

            // Mark in-flight via structural walk
            mark_shm_bytes_in_flight(&container);

            // Should be in-flight now
            assert_eq!(meta.state(), SlotState::InFlight);

            // Clean up
            std::mem::forget(container);
            pool.free(handle).expect("free");
        });
    }

    #[test]
    fn test_shm_bytes_alloc_and_free() {
        let (_region, pool) = create_test_pool();

        SHM_POOL.sync_scope(Arc::clone(&pool), || {
            // Allocate
            let bytes = ShmBytes::alloc(32).expect("alloc");
            assert_eq!(bytes.len(), 32);
            assert!(!bytes.is_empty());

            // Check we can access the slice
            let slice = bytes.as_slice().expect("as_slice");
            assert_eq!(slice.len(), 32);

            // Explicit free should work
            bytes.free().expect("free");
        });
    }

    #[test]
    fn test_shm_bytes_write_and_read() {
        let (_region, pool) = create_test_pool();

        SHM_POOL.sync_scope(Arc::clone(&pool), || {
            let mut bytes = ShmBytes::alloc(16).expect("alloc");

            // Write data
            let data = b"hello, shm!";
            bytes.as_mut_slice().expect("as_mut_slice")[..data.len()].copy_from_slice(data);

            // Read it back
            let slice = bytes.as_slice().expect("as_slice");
            assert_eq!(&slice[..data.len()], data);
        });
    }

    #[test]
    fn test_shm_bytes_drop_in_context() {
        let (_region, pool) = create_test_pool();

        SHM_POOL.sync_scope(Arc::clone(&pool), || {
            // Allocate and let it drop in context
            let bytes = ShmBytes::alloc(32).expect("alloc");
            let _handle = bytes.handle();
            drop(bytes);

            // Slot should be free, so we can allocate again and potentially get same slot
            // (or at least not run out of slots)
            let bytes2 = ShmBytes::alloc(32).expect("alloc after drop");
            // Should succeed - slot was freed
            drop(bytes2);
        });
    }

    #[test]
    fn test_shm_bytes_drop_outside_context() {
        let (_region, pool) = create_test_pool();

        let bytes = SHM_POOL.sync_scope(Arc::clone(&pool), || {
            ShmBytes::alloc(32).expect("alloc")
        });

        // Drop outside context - should silently do nothing (no panic)
        // In production, crash recovery would reclaim this slot
        drop(bytes);

        // We can still use the pool
        SHM_POOL.sync_scope(Arc::clone(&pool), || {
            let _bytes = ShmBytes::alloc(32).expect("alloc still works");
        });
    }

    #[test]
    fn test_shm_bytes_no_context_error() {
        // Without SHM_POOL set, alloc should fail
        let result = ShmBytes::alloc(32);
        assert!(matches!(result, Err(ShmError::NoContext)));
    }

    #[test]
    fn test_shm_bytes_deref() {
        let (_region, pool) = create_test_pool();

        SHM_POOL.sync_scope(Arc::clone(&pool), || {
            let mut bytes = ShmBytes::alloc(8).expect("alloc");
            bytes.as_mut_slice().expect("as_mut_slice").copy_from_slice(b"test1234");

            // Deref should give us the slice
            let slice: &[u8] = &*bytes;
            assert_eq!(slice, b"test1234");
        });
    }

    #[test]
    fn test_patch_shm_bytes_in_struct() {
        use facet::Facet;

        #[derive(Facet, Debug)]
        struct TestStruct {
            id: u32,
            data: ShmBytes,
            name: String,
        }

        let (_region, pool) = create_test_pool();

        SHM_POOL.sync_scope(Arc::clone(&pool), || {
            // Create a struct with ShmBytes
            let mut original = TestStruct {
                id: 42,
                data: ShmBytes::alloc(64).expect("alloc"),
                name: "test".to_string(),
            };
            // Set a specific length to verify it's transmitted
            original.data.len = 50;

            let original_handle = original.data.handle();

            // Serialize
            let bytes = facet_postcard::to_vec(&original).expect("serialize");

            // Deserialize - ShmBytes should have the correct length from wire format
            let mut decoded: TestStruct =
                facet_postcard::from_slice(&bytes).expect("deserialize");

            // Length should be correct immediately after deserialization (from ShmBytesWire)
            assert_eq!(decoded.data.len, 50);

            // Patch to claim ownership (but length should still be 50)
            patch_shm_bytes(&mut decoded);

            // After patching, handle should match and len should still be correct
            assert_eq!(decoded.data.handle().class_idx, original_handle.class_idx);
            assert_eq!(decoded.data.handle().slot_idx, original_handle.slot_idx);
            assert_eq!(decoded.data.handle().generation, original_handle.generation);
            // Length is from wire format, not slot size
            assert_eq!(decoded.data.len, 50);

            assert_eq!(decoded.id, 42);
            assert_eq!(decoded.name, "test");
        });
    }

    #[test]
    fn test_patch_shm_bytes_in_option() {
        use facet::Facet;

        #[derive(Facet, Debug)]
        struct TestOptional {
            data: Option<ShmBytes>,
        }

        let (_region, pool) = create_test_pool();

        SHM_POOL.sync_scope(Arc::clone(&pool), || {
            // Test with Some
            let original = TestOptional {
                data: Some(ShmBytes::alloc(64).expect("alloc")),
            };

            let bytes = facet_postcard::to_vec(&original).expect("serialize");
            let mut decoded: TestOptional =
                facet_postcard::from_slice(&bytes).expect("deserialize");

            patch_shm_bytes(&mut decoded);

            assert!(decoded.data.is_some());
            assert_eq!(decoded.data.as_ref().unwrap().len, 64);

            // Test with None
            let original_none = TestOptional { data: None };
            let bytes_none = facet_postcard::to_vec(&original_none).expect("serialize");
            let mut decoded_none: TestOptional =
                facet_postcard::from_slice(&bytes_none).expect("deserialize");

            patch_shm_bytes(&mut decoded_none);
            assert!(decoded_none.data.is_none());
        });
    }

    #[test]
    fn test_patch_shm_bytes_in_vec() {
        use facet::Facet;

        #[derive(Facet, Debug)]
        struct TestVec {
            items: Vec<ShmBytes>,
        }

        let (_region, pool) = create_test_pool();

        SHM_POOL.sync_scope(Arc::clone(&pool), || {
            let mut original = TestVec {
                items: vec![
                    ShmBytes::alloc(64).expect("alloc"),
                    ShmBytes::alloc(256).expect("alloc"),
                ],
            };
            // Set specific lengths to verify they're transmitted
            original.items[0].len = 30;
            original.items[1].len = 200;

            let bytes = facet_postcard::to_vec(&original).expect("serialize");
            let mut decoded: TestVec =
                facet_postcard::from_slice(&bytes).expect("deserialize");

            // Lengths should be correct immediately after deserialization (from ShmBytesWire)
            assert_eq!(decoded.items[0].len, 30);
            assert_eq!(decoded.items[1].len, 200);

            patch_shm_bytes(&mut decoded);

            // After patching - lengths still correct from wire format
            assert_eq!(decoded.items.len(), 2);
            assert_eq!(decoded.items[0].len, 30);
            assert_eq!(decoded.items[1].len, 200);
        });
    }

    #[test]
    fn test_shm_bytes_mark_in_flight() {
        use shm_primitives::SlotState;

        let (_region, pool) = create_test_pool();

        SHM_POOL.sync_scope(Arc::clone(&pool), || {
            let bytes = ShmBytes::alloc(32).expect("alloc");
            let handle = bytes.handle();

            // Verify initial state
            let meta = pool
                .slot_meta_ext(handle.class_idx as usize, handle.extent_idx as usize, handle.slot_idx)
                .expect("meta");
            assert_eq!(meta.state(), SlotState::Allocated);

            // Mark in-flight
            bytes.mark_in_flight().expect("mark_in_flight");
            assert_eq!(meta.state(), SlotState::InFlight);

            // Now we should free via pool.free() not free_allocated()
            pool.free(handle).expect("free");
        });
    }

    #[test]
    fn test_shm_bytes_ownership_transfer() {
        use shm_primitives::SlotState;

        let (_region, pool) = create_test_pool();

        // Sender (peer 1) allocates and marks in-flight
        let handle = SHM_POOL.sync_scope(Arc::clone(&pool), || {
            SHM_LOCAL_PEER_ID.sync_scope(1u8, || {
                let bytes = ShmBytes::alloc(64).expect("alloc");
                let handle = bytes.handle();

                // Verify owner is peer 1
                let meta = pool
                    .slot_meta_ext(handle.class_idx as usize, handle.extent_idx as usize, handle.slot_idx)
                    .expect("meta");
                assert_eq!(meta.owner(), 1);

                // Mark in-flight before sending
                bytes.mark_in_flight().expect("mark_in_flight");
                assert_eq!(meta.state(), SlotState::InFlight);

                // Forget the ShmBytes (ownership transfers to receiver)
                std::mem::forget(bytes);
                handle
            })
        });

        // Receiver (peer 2) claims the buffer
        SHM_POOL.sync_scope(Arc::clone(&pool), || {
            SHM_LOCAL_PEER_ID.sync_scope(2u8, || {
                // Simulate what patch_shm_bytes does: claim ownership
                pool.claim_in_flight(handle, 2).expect("claim");

                let meta = pool
                    .slot_meta_ext(handle.class_idx as usize, handle.extent_idx as usize, handle.slot_idx)
                    .expect("meta");
                assert_eq!(meta.state(), SlotState::Allocated);
                assert_eq!(meta.owner(), 2); // Owner changed to peer 2

                // Now receiver can use and eventually free
                pool.free_allocated(handle).expect("free");
            })
        });
    }

    #[test]
    fn test_shm_bytes_alloc_uses_local_peer_id() {
        let (_region, pool) = create_test_pool();

        // Allocate with peer ID 5
        let handle = SHM_POOL.sync_scope(Arc::clone(&pool), || {
            SHM_LOCAL_PEER_ID.sync_scope(5u8, || {
                let bytes = ShmBytes::alloc(32).expect("alloc");
                let h = bytes.handle();
                std::mem::forget(bytes); // Don't free yet
                h
            })
        });

        // Verify owner is peer 5
        let meta = pool
            .slot_meta_ext(handle.class_idx as usize, handle.extent_idx as usize, handle.slot_idx)
            .expect("meta");
        assert_eq!(meta.owner(), 5);

        // Clean up
        pool.free_allocated(handle).expect("free");
    }
}
