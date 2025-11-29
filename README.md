# AGTerm (ag_term) — Python-facing PTY wrapper

This module exposes a single Python class, `AGTerm`, implemented in Rust with PyO3 and backed by a pseudo-terminal (PTY) via `ptyprocess`. It’s meant for “agentic” usage where you want **reliable command/response boundaries** in interactive REPLs (shells, debuggers, etc.).

---

## Quick start

### Interactive REPL mode (prompt/marker-based)
Use this when the target program is a REPL and has a stable prompt (or you can set one).

```python
from ag_term import AGTerm

env = AGTerm("bash", True, ready_markers=["$ ", "# ", "__AGPROMPT__ "])

# Wait for the initial shell prompt
print(env.read_until_ready(timeout_ms=8000))

# Make the prompt deterministic for robust parsing
print(env.send_and_read_until_ready('export PS1="__AGPROMPT__ "', timeout_ms=8000))

# Now round-trip commands
print(env.send_and_read_until_ready("echo hello", timeout_ms=8000))

env.close()
````

### Non-interactive read strategy (quiet-based)

Use this when you don’t have a stable prompt (or you’re running “batch-ish” commands) but still want to keep the process alive.

```python
from ag_term import AGTerm

env = AGTerm("bash", False)  # interactive=False => quiet-based reads by default
print(env.send_and_read_until_ready("echo hi", timeout_ms=8000, quiet_ms=80))
env.close()
```

---

## Python API

### Constructor

```python
AGTerm(command: str, interactive: bool, ready_markers: list[str] | None = None)
```

* `command`: executable to start (e.g., `"bash"`, `"gdb"`, `"python3"`)
* `interactive`:

  * `True`: read methods wait for **ready markers** (prompt tokens)
  * `False`: read methods wait for **quiet** (no output for `quiet_ms`)
* `ready_markers` (optional): list of strings that indicate “ready for next input”

  * Defaults to `["pwndbg> ", "(gdb) "]` if you don’t pass anything.
  * For shells, pass something like `["$ ", "# "]` plus your custom prompt marker.

---

### Methods you can call from Python

#### `set_ready_markers(ready_markers: list[str]) -> None`

Replace the ready markers at runtime.

```python
env.set_ready_markers(["__PROMPT__ "])
```

#### `read_until_ready(timeout_ms=None, max_output_bytes=None, settle_ms=None, quiet_ms=None) -> str`

Reads output until the environment is “ready”:

* If `interactive=True` and markers exist: stop when a marker is detected.
* Otherwise (`interactive=False`): stop when output goes quiet for `quiet_ms`.

Arguments:

* `timeout_ms` (default: 20000): total time budget
* `max_output_bytes` (default: 2MB): safety cap on returned output
* `settle_ms` (default: 200): after seeing a marker, read a little longer to catch trailing bytes
* `quiet_ms` (default: 80): only used for `interactive=False` (quiet-based)

#### `send_and_read_until_ready(input: str, timeout_ms=None, max_output_bytes=None, settle_ms=None, quiet_ms=None) -> str`

Writes `input + "\n"` to the PTY and then reads until ready (same readiness rules as above).

This is the main “agent loop” call.

#### `read_available(max_bytes=None) -> str`

Non-blocking drain of currently buffered output. Returns `""` if nothing is available.

#### `send_ctrl_c() -> None`

Sends ASCII ETX (`0x03`) to the PTY (Ctrl-C). Useful for interrupting long-running commands in REPLs.

#### `get_history() -> str`

Returns a bounded history buffer containing sanitized output seen so far.

#### `is_alive() -> bool`

Returns whether the child process is alive.

#### `is_interactive() -> bool`

Returns the `interactive` flag.

#### `get_initial_command() -> str`

Returns the initial command string used to spawn.

#### `reset() -> None`

Kills the process and respawns it using the same:

* `command`
* `interactive` flag
* current `ready_markers`

**Note:** if you were using a custom shell prompt (`PS1`), you must set it again after reset.

#### `close() -> None`

Stops the process.

---

## How it works (high level)

* A PTY process is spawned via `PtyProcess::spawn`.
* The PTY master handle is cloned:

  * one clone kept for writing (a persistent `BufWriter`)
  * one clone for reading (a dedicated rust thread)
* The reader thread continuously `read()`s raw bytes and pushes them into an MPSC channel.
* Read calls (`read_until_ready` / `send_and_read_until_ready`) pull from that channel using `recv_timeout()`

  * interactive mode: return when a **ready marker** is detected in the (sanitized) tail
  * non-interactive mode: return when **no output** arrives for `quiet_ms`

All output returned to Python is **sanitized**:

* ANSI escape codes stripped
* carriage returns normalized away
* most control characters removed (keeps `\n`, `\t`)

---

## Configurable defaults (in Rust)

These are constants in `src/lib.rs`:

* `DEFAULT_TIMEOUT_MS` (20_000)
* `DEFAULT_SETTLE_MS` (200)
* `DEFAULT_QUIET_MS` (80)
* `DEFAULT_MAX_OUTPUT_BYTES` (2 * 1024 * 1024)
* `DEFAULT_MAX_HISTORY_BYTES` (4 * 1024 * 1024)
* `DEFAULT_READY_MARKERS` (["pwndbg> ", "(gdb) "])

You can override most of these at runtime via method parameters, except history cap which is currently compile-time.

---

## Gotchas / important notes

### 1) Your markers must actually appear

If `interactive=True` and you wait for a marker that never shows up (wrong prompt, ANSI-colored prompt, etc.), you’ll hit `timeout_ms` and return partial output.

**Recommendation for shells:** include `"$ "` and `"# "` as always-on fallbacks, then set a deterministic prompt:

```bash
export PS1="__AGPROMPT__ "
```

### 2) Bash may echo your command

When using a PTY, many programs echo inputs. Your output may contain:

* the command you sent
* the command output
* the prompt marker

So tests/parsers should check for substrings rather than exact equality.

### 3) `interactive=False` does NOT make the process “one-shot”

It only changes the **read strategy** to quiet-based. The shell/REPL still stays alive and maintains state unless you close/reset it.

### 4) Quiet-based reads are heuristic

If a program prints output with long gaps, you must increase `quiet_ms` or you may return before the final lines.

### 5) Sanitization may affect exact byte matching

ANSI escape sequences and some control characters are removed. That’s good for LLM input, but if you need exact bytes, you’d need an alternate “raw” API.

### 6) PyO3 classes don’t accept random attribute assignment

Unless you explicitly enable a `__dict__`, you can’t do `env.foo = 1` in Python; tests should not monkeypatch instance attributes.

### 7) Concurrency expectations

This is not designed for multiple simultaneous readers. Treat `send_and_read_until_ready` as the single canonical way to interact.

---

## Recommended patterns for agentic LLM usage

* Set a deterministic prompt (or prompt marker) early.
* Use `send_and_read_until_ready()` for every tool call.
* Keep model input clean:

  * Don’t prepend metadata to tool output (the module returns only sanitized text).
* Use `get_history()` only for debugging / logging; avoid feeding massive histories to the model.

---

## Troubleshooting

### “Returns empty string”

* You called `read_available()` and nothing was buffered.
* Or you waited for a marker that never appears and output was fully consumed earlier.

### “Commands hang until timeout”

* Marker mismatch (`ready_markers` wrong)
* Prompt is colored/ANSI-styled and your marker string doesn’t match after sanitization
* You are using `interactive=True` on a program that never prints a prompt

### “Cuts off the last line”

* In marker mode: set `settle_ms` higher (e.g., 300–500)
* In quiet mode: set `quiet_ms` higher than the program’s output gaps

```python
env.send_and_read_until_ready(cmd, quiet_ms=400)
```

```
::contentReference[oaicite:0]{index=0}
```
