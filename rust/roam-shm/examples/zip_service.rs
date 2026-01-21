//! Example: Zero-copy file handling with ShmBytes
//!
//! This example demonstrates using `ShmBytes` for zero-copy transfer of file
//! contents between services. The scenario:
//!
//! 1. **FsService** (on guest) - Reads files from disk into `ShmBytes`
//! 2. **ZipService** (on host) - Parses "zip" files and provides handles for exploration
//!
//! The "zip" format here is simplified: a newline-delimited text file where each
//! line is a filename. For example:
//! ```text
//! first.txt
//! second.txt
//! third.txt
//! ```
//!
//! The key insight is that the file bytes are read once into shared memory,
//! and the `ZipService` takes ownership of that buffer. No copies are made
//! when passing the data between services - only a small handle (~10 bytes)
//! crosses the wire.
//!
//! Run with: `cargo run --example zip_service -p roam-shm --features tracing`

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use roam_shm::driver::{establish_guest, establish_multi_peer_host, ShmConnectionHandle};
use roam_shm::host::ShmHost;
use roam_shm::layout::{SegmentConfig, SizeClass};
use roam_shm::shm_bytes::{ShmBytes, SHM_LOCAL_PEER_ID, SHM_POOL};
use roam_shm::transport::ShmGuestTransport;
use roam_shm::var_slot_pool::VarSlotPool;

// ============================================================================
// Service Definitions
// ============================================================================

/// A handle to an opened "zip" file.
/// The actual data is held by the ZipService in shared memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, facet::Facet)]
pub struct ZipHandle {
    pub id: u64,
}

/// Result of parsing a zip file - either a handle or an error message.
#[derive(Debug, Clone, facet::Facet)]
#[repr(u8)]
pub enum ParseResult {
    Ok(ZipHandle),
    Err(String),
}

/// Result of listing files - either paths or an error message.
#[derive(Debug, Clone, facet::Facet)]
#[repr(u8)]
pub enum ListResult {
    Ok(Vec<String>),
    Err(String),
}

/// Filesystem service - reads files into shared memory buffers.
///
/// This service runs on the "guest" side and provides file I/O.
#[roam::service]
trait FsService {
    /// Read an entire file into an ShmBytes buffer.
    ///
    /// The returned buffer is allocated in shared memory - the caller
    /// takes ownership and can pass it to other services without copying.
    async fn read_all(&self, path: String) -> ShmBytes;
}

/// Zip archive service - parses and explores "zip" files.
///
/// This service runs on the "host" side. When you call `parse()`, it takes
/// ownership of the `ShmBytes` buffer and returns a handle you can use for
/// further operations.
#[roam::service]
trait ZipService {
    /// Parse a "zip" file from the given buffer.
    ///
    /// Takes ownership of the ShmBytes - the data stays in shared memory
    /// and is held by this service until the handle is closed.
    async fn parse(&self, data: ShmBytes) -> ParseResult;

    /// List all files in the archive.
    async fn list_files(&self, handle: ZipHandle) -> ListResult;

    /// Close the archive, freeing the underlying ShmBytes.
    async fn close(&self, handle: ZipHandle);
}

// ============================================================================
// Service Implementations
// ============================================================================

/// FsService implementation that reads real files.
#[derive(Clone)]
struct FsServiceImpl;

impl FsService for FsServiceImpl {
    async fn read_all(&self, path: String) -> ShmBytes {
        // Read the file contents
        let contents = tokio::fs::read(&path)
            .await
            .expect(&format!("failed to read {}", path));

        // Allocate an ShmBytes buffer and copy the data
        // (This is the only copy - all further transfers are zero-copy)
        let mut buf = ShmBytes::alloc(contents.len())
            .expect("failed to allocate ShmBytes");

        if let Some(slice) = buf.as_mut_slice() {
            slice.copy_from_slice(&contents);
        }

        println!("[FsService] Read {} bytes from {}", contents.len(), path);
        buf
    }
}

/// State for an opened "zip" archive.
struct OpenedZip {
    /// The ShmBytes buffer containing the file data.
    /// We take ownership of this when parse() is called.
    #[allow(dead_code)]
    data: ShmBytes,
    /// Parsed list of filenames.
    files: Vec<String>,
}

/// ZipService implementation with stateful archive management.
#[derive(Clone)]
struct ZipServiceImpl {
    /// Next handle ID to allocate.
    next_id: Arc<AtomicU64>,
    /// Map of open archives.
    archives: Arc<Mutex<HashMap<u64, OpenedZip>>>,
}

impl ZipServiceImpl {
    fn new() -> Self {
        Self {
            next_id: Arc::new(AtomicU64::new(1)),
            archives: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl ZipService for ZipServiceImpl {
    async fn parse(&self, data: ShmBytes) -> ParseResult {
        // Read the contents from ShmBytes
        let contents = match data.as_slice() {
            Some(s) => s,
            None => return ParseResult::Err("failed to access ShmBytes contents".into()),
        };

        // Parse as UTF-8 text (our "zip" format is newline-delimited filenames)
        let text = match std::str::from_utf8(contents) {
            Ok(s) => s,
            Err(e) => return ParseResult::Err(format!("invalid UTF-8: {}", e)),
        };

        // Parse each line as a filename
        let files: Vec<String> = text
            .lines()
            .filter(|line| !line.is_empty())
            .map(String::from)
            .collect();

        println!("[ZipService] Parsed archive with {} files", files.len());

        // Allocate a handle
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let handle = ZipHandle { id };

        // Store the archive - we take ownership of the ShmBytes here
        let opened = OpenedZip { data, files };
        self.archives.lock().unwrap().insert(id, opened);

        ParseResult::Ok(handle)
    }

    async fn list_files(&self, handle: ZipHandle) -> ListResult {
        let archives = self.archives.lock().unwrap();
        let opened = match archives.get(&handle.id) {
            Some(o) => o,
            None => return ListResult::Err(format!("unknown handle: {}", handle.id)),
        };

        println!("[ZipService] Listing {} files for handle {}", opened.files.len(), handle.id);
        ListResult::Ok(opened.files.clone())
    }

    async fn close(&self, handle: ZipHandle) {
        let mut archives = self.archives.lock().unwrap();
        if archives.remove(&handle.id).is_some() {
            println!("[ZipService] Closed archive {}", handle.id);
        } else {
            println!("[ZipService] Warning: unknown handle {}", handle.id);
        }
    }
}

// ============================================================================
// Example Setup
// ============================================================================

struct ExampleFixture {
    /// Handle for calling FsService (on guest)
    fs_client: FsServiceClient<ShmConnectionHandle>,
    /// Handle for calling ZipService (on host) 
    zip_client: ZipServiceClient<ShmConnectionHandle>,
    /// Pool for accessing ShmBytes outside dispatch context
    pool: Arc<VarSlotPool>,
    /// Temp directory containing the example "zip" file
    _dir: tempfile::TempDir,
    /// Path to the example zip file
    zip_path: PathBuf,
}

async fn setup_example() -> ExampleFixture {
    let dir = tempfile::tempdir().unwrap();
    let shm_path = dir.path().join("example.shm");

    // Create example "zip" file
    let zip_path = dir.path().join("example.zip.txt");
    std::fs::write(&zip_path, "first.txt\nsecond.txt\nthird.txt\n").unwrap();

    // Configure SHM segment with variable-size slot classes for ShmBytes
    let config = SegmentConfig {
        max_payload_size: 4096,
        var_slot_classes: Some(vec![
            SizeClass::new(64, 16),    // Small files
            SizeClass::new(256, 8),    // Medium files  
            SizeClass::new(1024, 4),   // Large files
            SizeClass::new(4096, 2),   // Very large files
        ]),
        ..SegmentConfig::default()
    };

    let mut host = ShmHost::create(&shm_path, config).unwrap();
    let pool = host.var_slot_pool().expect("should have var_slot_pool");

    // Add a peer (guest)
    let ticket = host
        .add_peer(roam_shm::spawn::AddPeerOptions {
            peer_name: Some("fs-service".to_string()),
            on_death: None,
            ..Default::default()
        })
        .unwrap();

    let peer_id = ticket.peer_id;
    let spawn_args = ticket.into_spawn_args();

    // === Guest side: FsService ===
    let fs_impl = FsServiceImpl;
    let fs_dispatcher = FsServiceDispatcher::new(fs_impl);

    let guest_transport = ShmGuestTransport::from_spawn_args(spawn_args).unwrap();
    let (guest_handle, guest_driver) = establish_guest(guest_transport, fs_dispatcher);

    // === Host side: ZipService ===
    let zip_impl = ZipServiceImpl::new();
    let zip_dispatcher = ZipServiceDispatcher::new(zip_impl);

    let (host_driver, mut handles, _) = establish_multi_peer_host(
        host,
        vec![(peer_id, zip_dispatcher)],
    );
    let host_handle = handles.remove(&peer_id).unwrap();

    // Spawn the drivers
    tokio::spawn(guest_driver.run());
    tokio::spawn(host_driver.run());

    // Create clients
    // - fs_client: calls into the guest (FsService)
    // - zip_client: calls into the host (ZipService)
    let fs_client = FsServiceClient::new(host_handle);
    let zip_client = ZipServiceClient::new(guest_handle);

    ExampleFixture {
        fs_client,
        zip_client,
        pool,
        _dir: dir,
        zip_path,
    }
}

// ============================================================================
// Main
// ============================================================================

#[tokio::main]
async fn main() {
    // Initialize tracing for debug output
    tracing_subscriber::fmt::init();

    println!("=== ShmBytes Example: Zero-Copy File Handling ===\n");

    let fixture = setup_example().await;

    println!("Step 1: Reading file via FsService...");
    println!("  Path: {}\n", fixture.zip_path.display());

    // Step 1: Read the file into ShmBytes (via FsService on guest)
    let shm_bytes = fixture.fs_client.read_all(fixture.zip_path.to_string_lossy().into_owned()).await.unwrap();

    // Inspect the ShmBytes (need to be in SHM context)
    SHM_POOL.sync_scope(fixture.pool.clone(), || {
        SHM_LOCAL_PEER_ID.sync_scope(0, || {
            let slice = shm_bytes.as_slice().expect("should have slice");
            println!("  Received ShmBytes with {} bytes", slice.len());
            println!("  Contents: {:?}\n", String::from_utf8_lossy(slice));
        });
    });

    println!("Step 2: Parsing archive via ZipService...\n");

    // Step 2: Parse the "zip" file (transfers ownership to ZipService)
    let parse_result = fixture.zip_client.parse(shm_bytes).await.unwrap();
    let handle = match parse_result {
        ParseResult::Ok(h) => h,
        ParseResult::Err(e) => panic!("Parse failed: {}", e),
    };
    println!("  Got handle: {:?}\n", handle);

    println!("Step 3: Listing files in archive...\n");

    // Step 3: List files in the archive
    let list_result = fixture.zip_client.list_files(handle).await.unwrap();
    let files = match list_result {
        ListResult::Ok(f) => f,
        ListResult::Err(e) => panic!("List failed: {}", e),
    };
    println!("  Files in archive:");
    for file in &files {
        println!("    - {}", file);
    }
    println!();

    println!("Step 4: Closing archive...\n");

    // Step 4: Close the archive (frees the ShmBytes)
    fixture.zip_client.close(handle).await.unwrap();
    println!("  Archive closed, ShmBytes freed.\n");

    println!("=== Example Complete ===");
    println!("\nKey points demonstrated:");
    println!("  - File was read once into shared memory (FsService)");
    println!("  - Buffer ownership transferred zero-copy to ZipService");
    println!("  - ZipService held the data while we explored it");
    println!("  - Data was freed when we closed the handle");

    // Give drivers time to clean up
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
}
