# ShmBytes Implementation Progress

## Ambition

Enable **zero-copy transfer of large binary buffers** between roam services over SHM transport. Instead of serializing bytes through postcard (which copies data), services pass handles to pre-allocated shared memory slots. The actual bytes never leave SHM - only a small handle crosses the wire.

## Motivating Use Case

Services handling large binary payloads:
- **Image processing**: Camera service → ML inference → Display
- **Audio/Video**: Capture → Encode → Stream
- **ML tensors**: Model outputs passed between pipeline stages

Without `ShmBytes`: Each service boundary copies the entire buffer through postcard serialization.
With `ShmBytes`: Only a 10-byte handle (`VarSlotHandle`) crosses the wire; data stays in place.

## Design Decisions

### 1. Facet Proxy Pattern (like Tx/Rx)
```rust
#[derive(Facet)]
#[facet(proxy = VarSlotHandle)]
pub struct ShmBytes {
    handle: VarSlotHandle,
    len: usize,
}
```
- Serializes as `VarSlotHandle` (class_idx, extent_idx, slot_idx, generation)
- After deserialization, `patch_shm_bytes()` walks the structure to fill in lengths

### 2. Task-Local Pool Context
```rust
task_local! {
    pub static SHM_POOL: Arc<VarSlotPool>;
}
```
- SHM transport sets this before dispatching to service methods
- `ShmBytes` methods (`alloc`, `as_slice`, `free`) access pool through task-local
- Avoids passing pool reference through every function

### 3. Best-Effort Drop + Crash Recovery Safety Net
```rust
impl Drop for ShmBytes {
    fn drop(&mut self) {
        // Try to free if in SHM context; silent no-op if not
        let _ = SHM_POOL.try_with(|pool| pool.free_allocated(self.handle));
    }
}
```
- Explicit `free()` method for error handling when needed
- Drop silently does nothing outside SHM context
- Crash recovery reclaims slots from dead peers anyway

### 4. SHM-Only (for now)
- `ShmBytes` only makes sense over SHM transport
- Other transports (stream, websocket) should error if they encounter `ShmBytes`
- Could add fallback serialization later if needed

## Current Status: ✅ COMPLETE

All core ShmBytes functionality is implemented and tested:
- 69 unit tests pass
- 18 driver integration tests pass (3 ignored - pre-existing deadlock issues unrelated to ShmBytes)
- E2E tests confirm ShmBytes works: host→guest, guest→host, and round-trip processing

### Key Design Decisions Implemented

1. **`Caller::patch_response` method** ([rust/roam-session/src/lib.rs](../rust/roam-session/src/lib.rs))
   - New method on `Caller` trait for transport-specific response patching
   - Default implementation calls `call_patch_hook`
   - Made `call_patch_hook` public for custom implementations

2. **`ShmConnectionHandle` wrapper** ([rust/roam-shm/src/driver.rs](../rust/roam-shm/src/driver.rs))
   - Wraps `ConnectionHandle` + `VarSlotPool` + `local_peer_id`
   - Implements `Caller` with custom `patch_response` that sets up SHM context
   - Also has `call_raw` delegation for raw RPC calls

3. **Updated establish functions**
   - `establish_guest` now returns `ShmConnectionHandle` instead of `ConnectionHandle`
   - `establish_multi_peer_host` now returns `HashMap<PeerId, ShmConnectionHandle>`

4. **Generated client code** ([rust/roam-macros/src/lib.rs](../rust/roam-macros/src/lib.rs))
   - Calls `Caller::patch_response` after `decode_response`
   - Patching happens BEFORE debug logging (to avoid facet_pretty triggering mark_in_flight)

5. **All test fixtures updated**
   - `TestFixture`, `MultiPeerFixture`, `ShmBytesFixture`, `TracingTestFixture` all use `ShmConnectionHandle`

## Implementation Progress

### ✅ Completed

1. **`ShmBytes` type** ([rust/roam-shm/src/shm_bytes.rs](../rust/roam-shm/src/shm_bytes.rs))
   - `#[facet(proxy = VarSlotHandle)]` for wire serialization
   - `alloc()`, `as_slice()`, `as_mut_slice()`, `free()`
   - `Deref` to `[u8]` for ergonomic access
   - Move-only (no Clone)

2. **Task-local `SHM_POOL`**
   - Holds `Arc<VarSlotPool>` for current dispatch context

3. **`patch_shm_bytes()` hydration**
   - Walks structures via facet reflection to fill in lengths
   - Handles nested structs, `Option<ShmBytes>`, `Vec<ShmBytes>`
   - Fixed bug: `Def::List` vtable needs container shape, not element shape
   - Also fixed same bug in `patch_channel_ids` (roam-session)

4. **`VarSlotHandle` derives `Facet`**
   - Required for proxy serialization

5. **Comprehensive tests** (15 tests)
   - Allocation/free lifecycle
   - Read/write operations  
   - Proxy serialization roundtrip
   - Drop behavior in/out of context
   - Patching in structs, Options, Vecs
   - Ownership tracking: `mark_in_flight()`, ownership transfer, peer ID allocation

6. **Host-side transport integration** ([rust/roam-shm/src/host.rs](../rust/roam-shm/src/host.rs), [rust/roam-shm/src/driver.rs](../rust/roam-shm/src/driver.rs))
   - `ShmHost` now stores `Option<Arc<VarSlotPool>>` when configured with `var_slot_classes`
   - Added `var_slot_pool()` accessor to `ShmHost`
   - `MultiPeerHostDriver` gets pool from host during build
   - `handle_incoming_request` wraps dispatch in `SHM_POOL.scope()` when pool is present

7. **Guest-side var_slot_pool support**
   - `ShmGuest` now supports segments with `var_slot_pool_offset != 0`
   - Added `var_slot_pool()` accessor to `ShmGuest`
   - `ShmGuestTransport` exposes `var_slot_pool()` for driver use
   - `ShmDriver` (guest-side) sets `SHM_POOL` task-local before dispatch
   - `VarSlotPool::from_segment()` reads size classes from existing segment headers

8. **Layout supports both fixed pools AND var_slot_pool**
   - Changed layout to always have fixed-size per-guest pools (for message payloads)
   - var_slot_pool is placed after fixed pools when configured (for ShmBytes)
   - Added `var_slot_class_count` field to segment header for guest reconstruction

9. **`PATCH_HOOK` task-local in roam-session** ([rust/roam-session/src/lib.rs](../rust/roam-session/src/lib.rs))
   - Added `PatchHook` type alias and `PATCH_HOOK` task-local
   - `dispatch_call` and `dispatch_call_infallible` call the hook inside the async block
   - Hook runs when transport task-locals (like `SHM_POOL`) are available
   - Enables transport-specific post-deserialization patching without circular dependencies

10. **`patch_shm_bytes_hook` function** ([rust/roam-shm/src/shm_bytes.rs](../rust/roam-shm/src/shm_bytes.rs))
    - Function pointer form of `patch_shm_bytes` for use with `PATCH_HOOK`
    - Both `ShmDriver` (guest) and `MultiPeerHostDriver` (host) now set both `SHM_POOL` and `PATCH_HOOK`

11. **Ownership tracking** ([rust/roam-shm/src/shm_bytes.rs](../rust/roam-shm/src/shm_bytes.rs), [rust/roam-shm/src/var_slot_pool.rs](../rust/roam-shm/src/var_slot_pool.rs))
    - Added `SHM_LOCAL_PEER_ID` task-local for tracking the local peer ID (0=host, 1-255=guests)
    - `ShmBytes::alloc()` now uses the local peer ID as owner
    - `ShmBytes::mark_in_flight()` transitions slot from `Allocated` to `InFlight`
    - `VarSlotPool::claim_in_flight()` transitions `InFlight → Allocated` with new owner
    - `patch_shm_bytes()` now calls `claim_in_flight()` to take ownership of received ShmBytes
    - `ShmDriver` and `MultiPeerHostDriver` set `SHM_LOCAL_PEER_ID` task-local before dispatch
    - 6 new tests for ownership tracking (3 in shm_bytes, 3 in var_slot_pool)

12. **Auto mark_in_flight on serialization** ([rust/roam-shm/src/shm_bytes.rs](../rust/roam-shm/src/shm_bytes.rs))
    - `TryFrom<&ShmBytes> for ShmBytesWire` now calls `mark_in_flight()` automatically
    - Logs warning if SHM_POOL not available (e.g., facet_pretty debug logging on client)
    - Serialization continues even on failure (transport would reject ShmBytes anyway)

13. **`ShmConnectionHandle` wrapper** ([rust/roam-shm/src/driver.rs](../rust/roam-shm/src/driver.rs))
    - Wraps `ConnectionHandle` + `VarSlotPool` + `local_peer_id`
    - Implements `Caller` with custom `patch_response` that sets up SHM context
    - `establish_guest` and `establish_multi_peer_host` return this type

14. **`Caller::patch_response` method** ([rust/roam-session/src/lib.rs](../rust/roam-session/src/lib.rs))
    - New method on `Caller` trait for transport-specific response patching
    - Default calls `call_patch_hook`; SHM overrides to set context first
    - Generated clients call this after `decode_response`

15. **`ShmBytesWire` wire format** ([rust/roam-shm/src/shm_bytes.rs](../rust/roam-shm/src/shm_bytes.rs))
    - New proxy type containing `VarSlotHandle` + `len: u32`
    - Transmits actual data length on the wire (not just slot size)
    - Receiver gets correct length immediately after deserialization

16. **`Caller` trait with `Send` futures** ([rust/roam-session/src/lib.rs](../rust/roam-session/src/lib.rs))
    - `Caller::call` now returns `impl Future<...> + Send`
    - `T: Facet<'static> + Send` bound added to support spawned tasks (e.g., tracing drain)
    - All `Caller` impls updated: `ConnectionHandle`, `ShmConnectionHandle`, `FramedClient`, `Client`

17. **Transport integration** (fully wired up!)
    - Host driver sets `SHM_POOL` and `SHM_LOCAL_PEER_ID` task-locals before dispatch
    - Guest driver sets `SHM_POOL` and `SHM_LOCAL_PEER_ID` task-locals before dispatch
    - `patch_shm_bytes()` called after deserialization via `PATCH_HOOK` (server side)
    - Client-side patching via `ShmConnectionHandle::patch_response`
    - Auto `mark_in_flight` on serialization
    - Ownership claimed during patching
    - Actual length transmitted via `ShmBytesWire` proxy type

18. **E2E tests** ([rust/roam-shm/tests/driver.rs](../rust/roam-shm/tests/driver.rs))
    - Test fixture `ShmBytesFixture` created
    - Test service `ShmBytesTestbed` with methods for creating/reading/processing ShmBytes
    - Tests pass: `shm_bytes_host_to_guest`, `shm_bytes_guest_to_host`, `shm_bytes_round_trip_processing`

19. **Example: zip_service** ([rust/roam-shm/examples/zip_service.rs](../rust/roam-shm/examples/zip_service.rs))
    - Demonstrates real-world usage of `ShmBytes` for zero-copy file handling
    - `FsService` (guest): reads files into `ShmBytes` buffers
    - `ZipService` (host): parses "zip" files, takes ownership of `ShmBytes`, provides handles for exploration
    - Shows stateful service holding `ShmBytes` across multiple RPC calls
    - Run with: `cargo run --example zip_service -p roam-shm --features tracing`

### 🔲 Future Enhancements (Not Critical)

1. **Error on non-SHM transports** (optional)
   - Stream/websocket could reject ShmBytes gracefully
   - Currently would serialize but receiver couldn't access the data (no shared segment)

## Files Changed

| File | Change |
|------|--------|
| `rust/roam-shm/src/shm_bytes.rs` | ShmBytes type, ShmBytesWire proxy, SHM_POOL, patch_shm_bytes, auto mark_in_flight |
| `rust/roam-shm/src/lib.rs` | Added shm_bytes module + exports |
| `rust/roam-shm/src/var_slot_pool.rs` | Added `#[derive(Facet)]` to VarSlotHandle, `from_segment()`, `claim_in_flight()` |
| `rust/roam-shm/src/host.rs` | Added `var_slot_pool` field/accessor |
| `rust/roam-shm/src/driver.rs` | **NEW** `ShmConnectionHandle`, updated establish functions, task-local scopes |
| `rust/roam-shm/src/guest.rs` | Added `var_slot_pool` field/accessor |
| `rust/roam-shm/src/transport.rs` | Added `var_slot_pool()` and `peer_id()` accessors |
| `rust/roam-shm/src/layout.rs` | Added `var_slot_class_count` to header |
| `rust/roam-session/src/lib.rs` | `PATCH_HOOK`, `Caller::patch_response`, `Caller::call` returns `Send` future, `call_raw_with_channels` public |
| `rust/roam-session/src/driver.rs` | `FramedClient` impl of `Caller` with `Send` future |
| `rust/roam-stream/src/driver.rs` | `Client` impl of `Caller` with `Send` future |
| `rust/roam-macros/src/lib.rs` | Generated client calls `Caller::patch_response` after decode |
| `rust/roam-shm/tests/driver.rs` | E2E tests for ShmBytes |
| `rust/roam-shm/examples/zip_service.rs` | **NEW** Example demonstrating zero-copy file handling with stateful services |
| `rust/roam-shm/Cargo.toml` | Added example entry and tokio features for example |

## Next Steps

1. **Error on non-SHM** - Make stream transport reject ShmBytes gracefully
2. **Consider suppressing `mark_in_flight` warnings** - These warnings from facet_pretty debug logging are noisy but harmless

## Known Issues

- **Pre-existing deadlock in slot exhaustion + streaming**: Tests `mixed_calls_with_slot_exhaustion` and `slot_exhaustion_should_not_corrupt_channel_state` are ignored due to a deadlock when slots are exhausted while guest handlers await streaming data. Not related to ShmBytes work.
- **facet_pretty triggers proxy conversion**: Debug logging of ShmBytes on client side calls `mark_in_flight()` where SHM_POOL isn't set. Currently just logs a warning. The warning is harmless - the slot is already InFlight at this point.
- Some tests are verbose, so to avoid wasting tokens please always use a filter when running tests with tracing feature enabled. Example: `| Select-String -Pattern "(FAILED|PASSED|error|^test result|running \d+ test)"`. You do not need to filter the output of `cargo build`.
- **Tracing tests can be flaky** when run in parallel - use `--test-threads=1` if needed.

## Additional reading: 

[AGENTS.md](../AGENTS.md)