# ToolForge

ToolForge is a small, policy-controlled JSONL tool harness for coding agents.
It gives an agent useful repository tools while keeping the execution boundary
explicit, inspectable, and easy to replay.

![Rust](https://img.shields.io/badge/Rust-2021-orange?logo=rust)
![Tests](https://img.shields.io/badge/tests-included-success)
![License](https://img.shields.io/badge/license-MIT-blue)

## Why it exists

An LLM can suggest a shell command, but the component that executes that
command should own policy. ToolForge separates those responsibilities:

```text
Agent decides -> JSONL request -> ToolForge checks policy -> tool result
```

The first release supports:

- `list_files`: enumerate text-relevant files while skipping hidden/build trees
- `read_file`: read a bounded UTF-8 file inside the configured workspace
- `search_code`: literal search with bounded results
- `run_test`: run an explicitly allow-listed executable without a shell

Every request has a structured response. An optional JSONL audit log records
tool name, call number, success, duration, and errors without storing file
contents or command output.

## Safety boundary

ToolForge is a policy layer, not an operating-system sandbox. It is designed to
run against a workspace the caller already trusts.

- Paths are canonicalized and must remain inside `--workspace`.
- Symlinks are skipped during file enumeration.
- Commands are executed without a shell and must use an allow-listed program.
- Per-call output, file size, timeout, and total-call limits are bounded.
- The harness never exposes the host filesystem outside the workspace.

Tests can still write files inside the workspace. Use a temporary checkout or a
container when the code under test is untrusted.

## Quick start

```bash
cargo test
cargo run -- serve --workspace . --audit audit.jsonl
```

Then send JSONL requests on stdin:

```json
{"tool":"list_files","max_results":20}
{"tool":"search_code","query":"TODO"}
{"tool":"read_file","path":"README.md"}
{"tool":"run_test","command":["cargo","test"],"timeout_ms":30000}
```

The process writes one JSON response per non-empty input line. It does not
modify source files.

## Python integration

The companion [RepoPilot](../repopilot) project can launch ToolForge as its
execution backend:

```bash
repopilot analyze --repo . --issue issue.md --harness ./target/debug/toolforge.exe
```

The protocol is intentionally plain JSONL so other agents and languages can
integrate without an SDK.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

## License

MIT
