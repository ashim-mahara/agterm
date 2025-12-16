use ptyprocess::PtyProcess;
use pyo3::prelude::*;
use pyo3::types::PyModule;

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::process::Command;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const DEFAULT_TIMEOUT_MS: u64 = 20_000;
const DEFAULT_SETTLE_MS: u64 = 200;
const DEFAULT_QUIET_MS: u64 = 80;

const DEFAULT_MAX_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
const DEFAULT_MAX_HISTORY_BYTES: usize = 4 * 1024 * 1024;

const DEFAULT_READY_MARKERS: [&str; 2] = ["pwndbg> ", "(gdb) "];

#[pyclass(unsendable)]
struct AGTerm {
    process: PtyProcess,
    writer: Mutex<BufWriter<File>>,
    rx: Mutex<Receiver<Vec<u8>>>,
    reader_handle: Mutex<Option<JoinHandle<()>>>,

    command: String,
    interactive: bool,

    // Markers that indicate the tool is ready for the next command (prompt tokens).
    ready_markers: Mutex<Vec<Vec<u8>>>,

    history: Mutex<VecDeque<u8>>,
    max_history_bytes: usize,
}

#[pymethods]
impl AGTerm {
    /// ready_markers is optional; if not provided, defaults to ["pwndbg> ", "(gdb) "]
    /// If interactive=false, marker waiting is skipped and reads use "quiet" detection instead.
    #[new]
    #[pyo3(signature = (command, interactive, ready_markers=None))]
    fn new(command: String, interactive: bool, ready_markers: Option<Vec<String>>) -> PyResult<Self> {
        let t = Self::spawn(command, interactive, DEFAULT_MAX_HISTORY_BYTES)?;

        let markers = ready_markers.unwrap_or_else(|| {
            DEFAULT_READY_MARKERS.iter().map(|s| s.to_string()).collect()
        });
        *t.ready_markers.lock().unwrap() = markers.into_iter().map(|s| s.into_bytes()).collect();

        Ok(t)
    }

    pub fn set_ready_markers(&self, ready_markers: Vec<String>) -> PyResult<()> {
        let mut p = self
            .ready_markers
            .lock()
            .map_err(|_| pyo3::exceptions::PyRuntimeError::new_err("ready_markers lock poisoned"))?;
        p.clear();
        for s in ready_markers {
            p.push(s.into_bytes());
        }
        Ok(())
    }

    /// Read until ready marker appears (interactive=true) OR until output goes quiet (interactive=false).
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

        py.detach(|| {
            let raw = if self.should_wait_for_markers() {
                let (raw, _tr, _seen, _which) =
                    self.read_until_ready_inner(timeout_ms, max_output_bytes, settle_ms);
                raw
            } else {
                let (raw, _tr) = self.read_until_quiet_inner(timeout_ms, quiet_ms, max_output_bytes);
                raw
            };
            Ok(sanitize_for_agent(&raw))
        })
    }

    /// Send input and read until ready marker returns (interactive=true) OR until output goes quiet (interactive=false).
    #[pyo3(signature = (input, timeout_ms=None, max_output_bytes=None, settle_ms=None, quiet_ms=None))]
    pub fn send_and_read_until_ready(
        &mut self,
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

        py.detach(|| {
            let raw = if self.should_wait_for_markers() {
                let (raw, _tr, _seen, _which) =
                    self.read_until_ready_inner(timeout_ms, max_output_bytes, settle_ms);
                raw
            } else {
                let (raw, _tr) = self.read_until_quiet_inner(timeout_ms, quiet_ms, max_output_bytes);
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

        let rx = self
            .rx
            .lock()
            .map_err(|_| pyo3::exceptions::PyRuntimeError::new_err("rx lock poisoned"))?;

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

        self.append_history(&out);
        Ok(sanitize_for_agent(&out))
    }

    pub fn send_ctrl_c(&mut self) -> PyResult<()> {
        self.write_bytes(&[0x03]) // ETX
    }

    pub fn get_history(&self) -> PyResult<String> {
        let hist = self
            .history
            .lock()
            .map_err(|_| pyo3::exceptions::PyRuntimeError::new_err("history lock poisoned"))?;
        let (a, b) = hist.as_slices();
        Ok(format!("{}{}", sanitize_for_agent(a), sanitize_for_agent(b)))
    }

    pub fn is_alive(&self) -> PyResult<bool> {
        self.process
            .is_alive()
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(format!("is_alive failed: {e}")))
    }

    pub fn is_interactive(&self) -> PyResult<bool> {
        Ok(self.interactive)
    }

    pub fn get_initial_command(&self) -> PyResult<String> {
        Ok(self.command.clone())
    }

    pub fn close(&mut self) -> PyResult<()> {
        self.close_inner()
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(format!("close failed: {e}")))
    }

    pub fn reset(&mut self) -> PyResult<()> {
        let cmd = self.command.clone();
        let interactive = self.interactive;
        let max_hist = self.max_history_bytes;
        let markers = self.ready_markers.lock().unwrap().clone();

        let _ = self.close_inner();

        let new_self = Self::spawn(cmd, interactive, max_hist)?;
        *new_self.ready_markers.lock().unwrap() = markers;
        let _ = new_self.read_available(Some(256 * 1024));

        *self = new_self;
        Ok(())
    }
}

impl AGTerm {
    fn should_wait_for_markers(&self) -> bool {
        if !self.interactive {
            return false;
        }
        let markers = self.ready_markers.lock().unwrap();
        !markers.is_empty()
    }

    fn spawn(command: String, interactive: bool, max_history_bytes: usize) -> PyResult<Self> {
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

        let (tx, rx): (Sender<Vec<u8>>, Receiver<Vec<u8>>) = channel();
        let reader_handle = spawn_reader_thread(reader, tx);

        Ok(Self {
            process,
            writer: Mutex::new(writer),
            rx: Mutex::new(rx),
            reader_handle: Mutex::new(Some(reader_handle)),
            command,
            interactive,
            ready_markers: Mutex::new(Vec::new()),
            history: Mutex::new(VecDeque::with_capacity(64 * 1024)),
            max_history_bytes,
        })
    }

    fn close_inner(&mut self) -> std::io::Result<()> {
        let _ = self.process.exit(true);
        if let Some(h) = self.reader_handle.lock().ok().and_then(|mut g| g.take()) {
            let _ = h.join();
        }
        Ok(())
    }

    fn writer_lock(&self) -> PyResult<MutexGuard<'_, BufWriter<File>>> {
        self.writer
            .lock()
            .map_err(|_| pyo3::exceptions::PyRuntimeError::new_err("writer lock poisoned"))
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> PyResult<()> {
        let mut w = self.writer_lock()?;
        w.write_all(bytes)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(format!("write failed: {e}")))?;
        w.flush()
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(format!("flush failed: {e}")))?;
        Ok(())
    }

    fn write_to_stream(&mut self, input: &str) -> PyResult<()> {
        let mut s = input.as_bytes().to_vec();
        s.push(b'\n');
        self.write_bytes(&s)
    }

    fn append_history(&self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let mut hist = self.history.lock().unwrap();
        for &b in data {
            hist.push_back(b);
        }
        while hist.len() > self.max_history_bytes {
            hist.pop_front();
        }
    }

    fn read_until_quiet_inner(
        &self,
        timeout_ms: u64,
        quiet_ms: u64,
        max_output_bytes: usize,
    ) -> (Vec<u8>, bool) {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let quiet = Duration::from_millis(quiet_ms);

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

            let chunk_opt = {
                let rx = self.rx.lock().unwrap();
                match rx.recv_timeout(slice_wait) {
                    Ok(chunk) => Some(chunk),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
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

        self.append_history(&out);
        (out, truncated)
    }

    fn read_until_ready_inner(
        &self,
        timeout_ms: u64,
        max_output_bytes: usize,
        settle_ms: u64,
    ) -> (Vec<u8>, bool, bool, Option<Vec<u8>>) {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let settle = Duration::from_millis(settle_ms);

        let markers = self.ready_markers.lock().unwrap().clone();

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

            let chunk_opt = {
                let rx = self.rx.lock().unwrap();
                match rx.recv_timeout(slice_wait) {
                    Ok(chunk) => Some(chunk),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
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

        if saw_marker && !truncated {
            let settle_deadline = Instant::now() + settle;
            loop {
                if out.len() >= max_output_bytes {
                    truncated = true;
                    break;
                }
                if Instant::now() >= settle_deadline {
                    break;
                }
                let chunk_opt = {
                    let rx = self.rx.lock().unwrap();
                    match rx.recv_timeout(Duration::from_millis(25)) {
                        Ok(chunk) => Some(chunk),
                        Err(RecvTimeoutError::Timeout) => None,
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
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

        self.append_history(&out);
        (out, truncated, saw_marker, which)
    }
}

impl Drop for AGTerm {
    fn drop(&mut self) {
        let _ = self.close_inner();
    }
}

fn spawn_reader_thread(mut reader: BufReader<File>, tx: Sender<Vec<u8>>) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("agterm_reader".to_string())
        .spawn(move || {
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

            // CSI: ESC [ ... final-byte(@..~)
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

            // OSC: ESC ] ... BEL or ESC \
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

            // DCS / PM / APC: ESC P / ESC ^ / ESC _ ... ESC \
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
        if b < 0x20 && b != b'\n' && b != b'\t' {
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
