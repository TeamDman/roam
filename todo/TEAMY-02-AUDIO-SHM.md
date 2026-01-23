# Zero-Copy Audio Recording with ShmBytes

## Ambition

Enable **zero-copy audio recording** where WASAPI capture writes directly into ShmBytes, eliminating the intermediate `Vec<u8>` buffer and final copy during `drain_to_wav()`.

## Motivating Use Case

Real-time audio capture for:
- **Voice recording**: Microphone → ShmBytes → WAV file (or transcription service)
- **Audio processing pipelines**: Capture → Effects → Output
- **Low-latency streaming**: Capture directly into shared memory for IPC

Current flow (1 copy):
```
WASAPI → Vec<u8> (recording thread) → copy to ShmBytes (drain_to_wav) → FsService
```

Target flow (0 copies):
```
WASAPI → ShmBytes directly → FsService (or other consumer)
```

## Current Implementation Status

### ✅ Working (with 1 copy)

1. **MicrophoneService** (`src/services/mic_service.rs`)
   - `start_recording(device_id)` → spawns `std::thread` for WASAPI capture
   - `stop_recording(device_id)` → signals thread via channel, joins
   - `drain_to_wav(device_id)` → allocates ShmBytes, writes WAV header + copies audio data
   - `list()` → enumerates audio input devices

2. **Direct WAV header writing** (eliminated hound dependency)
   - `write_wav_header()` writes 44-byte (PCM) or 68-byte (Extensible) header directly
   - Raw PCM samples copied after header - no sample-by-sample conversion needed
   - Supports both PCMWAVEFORMAT and WAVEFORMATEXTENSIBLE

3. **FsService** (`src/services/fs_service.rs`)
   - `open(path, options)` → returns FileHandle
   - `write(handle, ShmBytes)` → writes directly from SHM to disk
   - `close(handle)` → closes file

4. **ServiceRuntime** (`src/services/runtime.rs`)
   - Sets up roam-shm transport with VarSlotPool
   - MicrophoneService on host side, FsService on guest side
   - Clients get `ShmConnectionHandle` for ShmBytes-aware RPC

5. **CLI command** (`src/cli/command/mic/record/`)
   - `teamy-windows mic record --id <device> --duration 2s --output-path out.wav`
   - Demonstrates full flow: start → wait → stop → drain → write

### Current Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│ Recording Thread (std::thread)                                  │
│   - COM initialized (required for WASAPI)                       │
│   - Captures audio in loop                                      │
│   - Writes to Vec<u8>                          ← PROBLEM        │
│   - Returns Vec when stopped                                    │
└─────────────────────────────────────────────────────────────────┘
                              ↓
┌─────────────────────────────────────────────────────────────────┐
│ drain_to_wav()                                                  │
│   1. ShmBytes::alloc(header_size + audio_len)                   │
│   2. write_wav_header() into slice[..header_size]               │
│   3. slice[header_size..].copy_from_slice(&audio_data) ← COPY   │
│   4. Return AudioSegment { bytes: ShmBytes, ... }               │
└─────────────────────────────────────────────────────────────────┘
```

## Design Challenge: Zero-Copy Recording

### Problem 1: COM Thread Affinity

WASAPI requires COM initialization on the thread that uses it. Currently we use `std::thread`, but this means:
- No access to tokio runtime features
- No SHM task-locals (`SHM_POOL`, `SHM_LOCAL_PEER_ID`)

**Solution**: Use `tokio::task::spawn_blocking` instead. The blocking pool threads can have COM initialized, and we can pass the `VarSlotPool` explicitly rather than relying on task-locals.

### Problem 2: WAV Header Needs Total Size

WAV format requires the data size in the header (bytes 4-8 and 40-44). Options:

**Option A: Reserve header space, fill in later**
```rust
struct RecordingSession {
    shm_bytes: ShmBytes,           // Pre-allocated with max size
    write_offset: usize,           // Current position (after header)
    header_size: usize,            // 44 or 68 bytes reserved
    format_info: AudioFormat,
}
```
- `start_recording`: Allocate ShmBytes with configurable max size, reserve header
- WASAPI writes at `&mut slice[header_size + write_offset..]`
- `stop_recording`: Fill in header with actual size
- `drain_to_wav`: Return the ShmBytes (may have unused trailing space)

**Option B: Chunked ShmBytes**
- Allocate 1MB chunks as needed
- On drain, allocate final ShmBytes = header + sum(chunks), copy once

**Option C: Raw streaming (no WAV)**
- Return raw PCM in ShmBytes, let consumer handle format
- Good for real-time pipelines, not for file output

### Recommended: Option A with Reserved Header

Simplest path to zero-copy during capture:

1. **Pre-allocate** ShmBytes with generous size (e.g., 10MB = ~55 seconds at 48kHz/stereo/32-bit)
2. **Reserve** 68 bytes at start for header (use larger format to be safe)
3. **WASAPI writes** directly into `shm_bytes.as_mut_slice()[68..]`
4. **On stop**, write header bytes at `[0..68]` with actual data size
5. **Return** ShmBytes (trailing unused bytes are harmless - header has correct size)

Trade-off: Some wasted SHM slot space. But slots are reusable, and this achieves true zero-copy.

## Implementation Plan

### Phase 1: Refactor to spawn_blocking (prep work)
- [ ] Replace `std::thread::spawn` with `tokio::task::spawn_blocking`
- [ ] Pass `Arc<VarSlotPool>` explicitly to blocking task
- [ ] Verify COM still works in tokio blocking threads

### Phase 2: Direct ShmBytes capture
- [ ] `start_recording` allocates ShmBytes, stores in session
- [ ] Blocking task receives `&mut [u8]` slice from ShmBytes
- [ ] WASAPI writes directly into slice
- [ ] Track write position atomically or via channel

### Phase 3: Header finalization
- [ ] On stop, calculate actual data size
- [ ] Write WAV header into reserved space
- [ ] Return ShmBytes via oneshot channel

### Phase 4: Configurable buffer size
- [ ] Accept max_duration or max_bytes parameter
- [ ] Default to reasonable size (e.g., 60 seconds)
- [ ] Error if recording exceeds buffer

## Files Involved

| File | Current State | Target State |
|------|--------------|--------------|
| `src/services/mic_service.rs` | std::thread + Vec<u8> | spawn_blocking + ShmBytes |
| `src/services/runtime.rs` | Sets up transport | May need to expose pool |
| `src/audio/audio_recording.rs` | Standalone record function | May be merged/removed |

## Open Questions

1. **Buffer overflow handling**: What if recording exceeds pre-allocated size?
   - Option: Stop recording and return partial data
   - Option: Allocate new ShmBytes and chain (like Option B)

2. **Multiple concurrent recordings**: Current design supports multiple devices. Need to ensure each gets its own ShmBytes.

3. **Cancellation**: If recording is cancelled, need to free the ShmBytes properly.

## Related Work

- [TEAMY-01-SHM-BYTES.md](./TEAMY-01-SHM-BYTES.md) - ShmBytes implementation (complete)
- [zip_service example](../rust/roam-shm/examples/zip_service.rs) - Demonstrates stateful service holding ShmBytes
