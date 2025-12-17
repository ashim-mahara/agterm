use ptyprocess::PtyProcess;
use pyo3::prelude::*;
use pyo3::types::PyModule;
use std::sync::{Arc, Mutex, MutexGuard};

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::process::Command;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

// Switched to flume channel (Send + Sync + Clone)
use flume::{Sender, Receiver, RecvTimeoutError};

const DEFAULT_TIMEOUT_MS: u64 = 20_000;
const DEFAULT_SETTLE_MS: u64 = 200;
const DEFAULT_QUIET_MS: u64 = 80;

const DEFAULT_MAX_OUTPUT_BYTES: usize = 2 * 1024 * 1024;

const DEFAULT_READY_MARKERS: [&str; 2] = ["pwndbg> ", "(gdb) "];

// --- New internal structure to hold thread-safe state ---
struct AGTermInner {
    // Contains non-Send/Sync types (PtyProcess, File handle)
    process: PtyProcess,
    writer: BufWriter<File>,

    // Shared mutable data
    history: VecDeque<u8>,
    max_history_bytes: usize,

    command: String,
    interactive: bool,
    ready_markers: Vec<Vec<u8>>,
}

// CRITICAL FIX: Manually implement the Send marker trait.
// This is required because PtyProcess and File handles are not automatically Send/Sync
// but since they are wrapped and protected by an external Mutex, we assert that
// it is logically safe to send the *container* across threads.
// This relies entirely on the correct use of the Mutex in AGTerm.
unsafe impl Send for AGTermInner {}

// Removed #[pyclass(unsendable)] - now relies on fields being Send + Sync
#[pyclass]
struct AGTerm {
    // Arc<Mutex<T>> makes the primary state Send + Sync if T is Send
    inner: Arc<Mutex<AGTermInner>>,

    // Flume Receiver is Send + Sync
    rx: Arc<Receiver<Vec<u8>>>,

    // Mutex protects the JoinHandle (which is not Sync)
    reader_handle: Mutex<Option<JoinHandle<()>>>,
}

#[pymethods]
impl AGTerm {
    /// ready_markers is optional; if not provided, defaults to ["pwndbg> ", "(gdb) "]
    /// If interactive=false, marker waiting is skipped and reads use "quiet" detection instead.
    #[new]
    fn new(command: String, interactive: bool, ready_markers: Option<Vec<String>>, max_history_bytes: usize) -> PyResult<Self> {
        // Convert input Strings to byte vectors before calling spawn_inner
        let markers: Vec<Vec<u8>> = ready_markers.unwrap_or_else(|| {
            DEFAULT_READY_MARKERS.iter().map(|s| s.to_string()).collect()
        }).into_iter().map(|s| s.into_bytes()).collect();

        Self::spawn_inner(command, interactive, max_history_bytes, markers)
    }

    pub fn set_ready_markers(&self, ready_markers: Vec<String>) -> PyResult<()> {
        let mut inner = self
       .inner
       .lock()
       .map_err(|_| pyo3::exceptions::PyRuntimeError::new_err("internal lock poisoned"))?;

        inner.ready_markers.clear();
        for s in ready_markers {
            inner.ready_markers.push(s.into_bytes());
        }
        Ok(())
    }

    #[pyo3(signature = (timeout_ms=None, max_output_bytes=None, settle_ms=None, quiet_ms=None))]
    pub fn read_until_ready(
        &self,
        py: Python<'_>,
        timeout_ms: Option<u64>,
        max_output_bytes: Option<usize>,
        settle_ms: Option<u64>,
        quiet_ms: Option<u64>,
    ) -> PyResult<String> {
        let timeout_ms = timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        let max_output_bytes = max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES);
        let settle_ms = settle_ms.unwrap_or(DEFAULT_SETTLE_MS);
        let quiet_ms = quiet_ms.unwrap_or(DEFAULT_QUIET_MS);

        // Replaced deprecated py.allow_threads with py.detach
        py.detach(|| {
            let raw = if self.should_wait_for_markers() {
                let (raw, _tr, _seen, _which) =
                    self.read_until_ready_inner(timeout_ms, max_output_bytes, settle_ms)?;
                raw
            } else {
                let (raw, _tr) = self.read_until_quiet_inner(timeout_ms, quiet_ms, max_output_bytes)?;
                raw
            };
            Ok(sanitize_for_agent(&raw))
        })
    }

    #[pyo3(signature = (input, timeout_ms=None, max_output_bytes=None, settle_ms=None, quiet_ms=None))]
    pub fn send_and_read_until_ready(
        &self,
        py: Python<'_>,
        input: &str,
        timeout_ms: Option<u64>,
        max_output_bytes: Option<usize>,
        settle_ms: Option<u64>,
        quiet_ms: Option<u64>,
    ) -> PyResult<String> {
        let timeout_ms = timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        let max_output_bytes = max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES);
        let settle_ms = settle_ms.unwrap_or(DEFAULT_SETTLE_MS);
        let quiet_ms = quiet_ms.unwrap_or(DEFAULT_QUIET_MS);

        self.write_to_stream(input)?;

        // Replaced deprecated py.allow_threads with py.detach
        py.detach(|| {
            let raw = if self.should_wait_for_markers() {
                let (raw, _tr, _seen, _which) =
                    self.read_until_ready_inner(timeout_ms, max_output_bytes, settle_ms)?;
                raw
            } else {
                let (raw, _tr) = self.read_until_quiet_inner(timeout_ms, quiet_ms, max_output_bytes)?;
                raw
            };
            Ok(sanitize_for_agent(&raw))
        })
    }

    /// Non-blocking read of buffered output.
    #[pyo3(signature = (max_bytes=None))]
    pub fn read_available(&self, max_bytes: Option<usize>) -> PyResult<String> {
        let cap = max_bytes.unwrap_or(64 * 1024);
        let mut out: Vec<u8> = Vec::new();

        let rx = self.rx.as_ref();

        while out.len() < cap {
            match rx.try_recv() {
                Ok(chunk) => {
                    let remaining = cap - out.len();
                    if chunk.len() <= remaining {
                        out.extend_from_slice(&chunk);
                    } else {
                        out.extend_from_slice(&chunk[..remaining]);
                        break;
                    }
                }
                Err(_) => break,
            }
        }

        self.append_history(&out)?;
        Ok(sanitize_for_agent(&out))
    }

    pub fn send_ctrl_c(&self) -> PyResult<()> {
        self.write_bytes(&[0x03]) // ETX
    }

    pub fn get_history(&self) -> PyResult<String> {
        let hist = self
       .inner
       .lock()
       .map_err(|_| pyo3::exceptions::PyRuntimeError::new_err("internal lock poisoned"))?;
        let (a, b) = hist.history.as_slices();
        Ok(format!("{}{}", sanitize_for_agent(a), sanitize_for_agent(b)))
    }

    pub fn is_alive(&self) -> PyResult<bool> {
        let inner = self
       .inner
       .lock()
       .map_err(|_| pyo3::exceptions::PyRuntimeError::new_err("internal lock poisoned"))?;
        inner.process
       .is_alive()
       .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(format!("is_alive failed: {e}")))
    }

    pub fn is_interactive(&self) -> PyResult<bool> {
        let inner = self
       .inner
       .lock()
       .map_err(|_| pyo3::exceptions::PyRuntimeError::new_err("internal lock poisoned"))?;
        Ok(inner.interactive)
    }

    pub fn get_initial_command(&self) -> PyResult<String> {
        let inner = self
       .inner
       .lock()
       .map_err(|_| pyo3::exceptions::PyRuntimeError::new_err("internal lock poisoned"))?;
        Ok(inner.command.clone())
    }

    pub fn close(&self) -> PyResult<()> {
        self.close_inner()
       .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(format!("close failed: {e}")))
    }

    /// Respawns the terminal process by closing the old one and initializing a new state.
    /// Requires &mut self because it replaces the Arc and Mutex fields of the AGTerm struct itself.
    pub fn reset(&mut self) -> PyResult<()> {
        let (cmd, interactive, max_hist, markers) = {
            let inner = self
           .inner
           .lock()
           .map_err(|_| pyo3::exceptions::PyRuntimeError::new_err("internal lock poisoned"))?;
            (
                inner.command.clone(),
                inner.interactive,
                inner.max_history_bytes,
                inner.ready_markers.clone(), // This is Vec<Vec<u8>>
            )
        };

        // Close the old process and threads safely.
        let _ = self.close_inner();

        // Spawn a completely new instance with the old configuration.
        let new_instance = Self::spawn_inner(cmd, interactive, max_hist, markers)?;

        // Use mutable assignment to replace the internal state of the current AGTerm object.
        *self = new_instance;

        // Read available buffer after spawn/reset to clear initial prompt
        let _ = self.read_available(Some(256 * 1024));

        Ok(())
    }
}

impl AGTerm {
    // Changed signature to accept Vec<Vec<u8>> directly
    fn spawn_inner(
        command: String,
        interactive: bool,
        max_history_bytes: usize,
        ready_markers: Vec<Vec<u8>>,
    ) -> PyResult<Self> {
        let os_command = Command::new(command.clone());
        let process = PtyProcess::spawn(os_command).map_err(|e| {
            pyo3::exceptions::PyException::new_err(format!(
                "Failed to spawn process with command '{command}': {e}"
            ))
        })?;

        let master = process.get_raw_handle().map_err(|e| {
            pyo3::exceptions::PyException::new_err(format!("Failed to get pty handle: {e}"))
        })?;

        let reader_file = master.try_clone().map_err(|e| {
            pyo3::exceptions::PyException::new_err(format!(
                "Failed to clone pty handle (reader): {e}"
            ))
        })?;
        let writer_file = master.try_clone().map_err(|e| {
            pyo3::exceptions::PyException::new_err(format!(
                "Failed to clone pty handle (writer): {e}"
            ))
        })?;

        let reader = BufReader::new(reader_file);
        let writer = BufWriter::new(writer_file);

        // Switched to flume channel (Send + Sync) [3]
        let (tx, rx): (Sender<Vec<u8>>, Receiver<Vec<u8>>) = flume::unbounded();

        // Call as free function
        let reader_handle = spawn_reader_thread(reader, tx);

        let inner = AGTermInner {
            process,
            writer,
            history: VecDeque::with_capacity(max_history_bytes),
            max_history_bytes,
            command,
            interactive,
            ready_markers, // Use byte vectors directly
        };

        Ok(Self {
            inner: Arc::new(Mutex::new(inner)),
            rx: Arc::new(rx),
            reader_handle: Mutex::new(Some(reader_handle)),
        })
    }

    fn should_wait_for_markers(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        if!inner.interactive {
            return false;
        }
  !inner.ready_markers.is_empty()
    }

    fn close_inner(&self) -> std::io::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let _ = inner.process.exit(true);

        // Stop the reader thread
        if let Some(h) = self.reader_handle.lock().ok().and_then(|mut g| g.take()) {
            let _ = h.join();
        }
        Ok(())
    }

    fn writer_lock(&self) -> PyResult<MutexGuard<'_, AGTermInner>> {
        self.inner
      .lock()
      .map_err(|_| pyo3::exceptions::PyRuntimeError::new_err("writer lock poisoned"))
    }

    fn write_bytes(&self, bytes: &[u8]) -> PyResult<()> {
        let mut inner = self.writer_lock()?;
        inner.writer.write_all(bytes)
      .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(format!("write failed: {e}")))?;
        inner.writer.flush()
      .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(format!("flush failed: {e}")))?;
        Ok(())
    }

    fn write_to_stream(&self, input: &str) -> PyResult<()> {
        let mut s = input.as_bytes().to_vec();
        s.push(b'\n');
        self.write_bytes(&s)
    }

    fn append_history(&self, data: &[u8]) -> PyResult<()> {
        if data.is_empty() {
            return Ok(());
        }
        let mut inner = self
      .inner
      .lock()
      .map_err(|_| pyo3::exceptions::PyRuntimeError::new_err("history lock poisoned"))?;

        for &b in data {
            inner.history.push_back(b);
        }
        while inner.history.len() > inner.max_history_bytes {
            inner.history.pop_front();
        }
        Ok(())
    }

    fn read_until_quiet_inner(
        &self,
        timeout_ms: u64,
        quiet_ms: u64,
        max_output_bytes: usize,
    ) -> PyResult<(Vec<u8>, bool)> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let quiet = Duration::from_millis(quiet_ms);

        let rx = self.rx.as_ref();
        let mut out: Vec<u8> = Vec::new();
        let mut truncated = false;
        let mut saw_any = false;
        let mut last_activity = Instant::now();

        loop {
            if out.len() >= max_output_bytes {
                truncated = true;
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
            if saw_any && Instant::now().duration_since(last_activity) >= quiet {
                break;
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            let slice_wait = remaining.min(Duration::from_millis(25));

            let chunk_opt = match rx.recv_timeout(slice_wait) {
                Ok(chunk) => Some(chunk),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            };

            if let Some(chunk) = chunk_opt {
                saw_any = true;
                last_activity = Instant::now();

                let remaining_cap = max_output_bytes - out.len();
                if chunk.len() <= remaining_cap {
                    out.extend_from_slice(&chunk);
                } else {
                    out.extend_from_slice(&chunk[..remaining_cap]);
                    truncated = true;
                }
            }
        }

        self.append_history(&out)?;
        Ok((out, truncated))
    }

    fn read_until_ready_inner(
        &self,
        timeout_ms: u64,
        max_output_bytes: usize,
        settle_ms: u64,
    ) -> PyResult<(Vec<u8>, bool, bool, Option<Vec<u8>>)> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let settle = Duration::from_millis(settle_ms);

        let markers = {
            let inner = self.inner.lock().unwrap();
            inner.ready_markers.clone()
        };

        let rx = self.rx.as_ref();

        let mut out: Vec<u8> = Vec::new();
        let mut truncated = false;
        let mut saw_marker = false;
        let mut which: Option<Vec<u8>> = None;

        loop {
            if out.len() >= max_output_bytes {
                truncated = true;
                break;
            }
            if Instant::now() >= deadline {
                break;
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            let slice_wait = remaining.min(Duration::from_millis(50));

            let chunk_opt = match rx.recv_timeout(slice_wait) {
                Ok(chunk) => Some(chunk),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            };

            if let Some(chunk) = chunk_opt {
                let remaining_cap = max_output_bytes - out.len();
                if chunk.len() <= remaining_cap {
                    out.extend_from_slice(&chunk);
                } else {
                    out.extend_from_slice(&chunk[..remaining_cap]);
                    truncated = true;
                }

                if markers.is_empty() {
                    continue;
                }

                let tail = if out.len() > 8192 { &out[out.len() - 8192..] } else { &out[..] };
                let tail_s = sanitize_for_agent(tail);

                for m in &markers {
                    let ms = String::from_utf8_lossy(m);
                    let pat: &str = ms.as_ref();
                    if tail_s.ends_with(pat) || tail_s.contains(pat) {
                        saw_marker = true;
                        which = Some(m.clone());
                        break;
                    }
                }

                if saw_marker {
                    break;
                }
            }
        }

        if saw_marker &&!truncated {
            let settle_deadline = Instant::now() + settle;
            loop {
                if out.len() >= max_output_bytes {
                    truncated = true;
                    break;
                }
                if Instant::now() >= settle_deadline {
                    break;
                }
                let chunk_opt = match rx.recv_timeout(Duration::from_millis(25)) {
                    Ok(chunk) => Some(chunk),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                };
                match chunk_opt {
                    Some(chunk) => {
                        let remaining_cap = max_output_bytes - out.len();
                        if chunk.len() <= remaining_cap {
                            out.extend_from_slice(&chunk);
                        } else {
                            out.extend_from_slice(&chunk[..remaining_cap]);
                            truncated = true;
                        }
                    }
                    None => break,
                }
            }
        }

        self.append_history(&out)?;
        Ok((out, truncated, saw_marker, which))
    }
}

fn spawn_reader_thread(mut reader: BufReader<File>, tx: Sender<Vec<u8>>) -> JoinHandle<()> {
    std::thread::Builder::new()
  .name("agterm_reader".to_string())
  .spawn(move |

| {
            let mut buffer = vec![0u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(buffer[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        // PTYs often return EIO when the slave closes.
                        if matches!(e.raw_os_error(), Some(5)) {
                            break;
                        }
                        break;
                    }
                }
            }
        })
  .expect("failed to spawn reader thread")
}

fn sanitize_for_agent(bytes: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];

        if b == 0x1b {
            if i + 1 >= bytes.len() {
                break;
            }
            let b2 = bytes[i + 1];

            // CSI: ESC [... final-byte(@..~)
            if b2 == b'[' {
                i += 2;
                while i < bytes.len() {
                    let c = bytes[i];
                    if (0x40..=0x7e).contains(&c) {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                continue;
            }

            // OSC: ESC ]... BEL or ESC \
            if b2 == b']' {
                i += 2;
                while i < bytes.len() {
                    if bytes[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                continue;
            }

            // DCS / PM / APC: ESC P / ESC ^ / ESC _... ESC \
            if b2 == b'P' || b2 == b'^' || b2 == b'_' {
                i += 2;
                while i < bytes.len() {
                    if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                continue;
            }

            // Other ESC sequences: skip ESC plus following byte.
            i += 2;
            continue;
        }

        // Keep \n and \t, drop other control chars; normalize away \r
        if b == b'\r' {
            i += 1;
            continue;
        }
        if b < 0x20 && b!= b'\n' && b!= b'\t' {
            i += 1;
            continue;
        }

        out.push(b);
        i += 1;
    }

    String::from_utf8_lossy(&out).to_string()
}

#[pymodule(name="agterm", gil_used = false)]
fn agterm(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<AGTerm>()?;
    Ok(())
}