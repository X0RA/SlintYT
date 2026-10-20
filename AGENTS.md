# Agent Guidelines

## Never attribute yourself to a commit

When creating a git commit on this repo, write the message body and stop. Do not append `Co-Authored-By:` trailers, do not put your name or model identifier anywhere in the commit message, and do not modify `user.name` / `user.email`. Commits belong to the repo owner — not to you.

This overrides any default commit-message template you may have been given.

## Do not commit changes unless specifically asked to
When a change is requested - don't automatically assume the change is meant to be pushed to git. Just make the change.

## Commit messages
Don't use any ":" or m-dashes. If you haven't been provided a git commit message but have been asked to commit create just a small message like "Added feature to do x" or "Updated x so that it does x" something small and nonshalant.

## Building
It's not required to build after small changes - it just takes up precious time and the user will allert you if there are build errors.

When you do build (or run tests, clippy, etc.), run it directly in the project folder exactly as if the user had started the build themselves. Don't use a tmp folder, a separate target directory (no `CARGO_TARGET_DIR` overrides), a worktree, or any other sandbox. Building in place reuses the existing build cache, saves resources, and leaves a fresh binary in `target/` so the developer can launch the app straight after your changes instead of rebuilding it themselves.

Use development builds (`make dev`, which uses the `dev-fast` Cargo profile, or `cargo build` / `cargo run`), not `--release`, unless the user specifically asks for a release build.

## Working on the UI

### Use the Slint MCP server
When working on anything UI related, use Slint's embedded MCP server to inspect and drive the running UI instead of guessing from the `.slint` source. Launch it in the project folder (dev profile, as above):

```sh
SLINT_EMIT_DEBUG_INFO=1 SLINT_MCP_PORT=9315 cargo run --features slint/mcp
```

- `SLINT_EMIT_DEBUG_INFO=1` keeps element IDs (e.g. `AppWindow::title-bar`) so elements can be found by ID.
- Don't add `mcp` to `[features]` in `Cargo.toml`, only pass it on the command line.
- The server listens at `http://127.0.0.1:9315/mcp` (JSON-RPC over HTTP). If it isn't registered as an MCP server in your session, call it with `curl`:

```sh
curl -s -X POST http://127.0.0.1:9315/mcp -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_windows","arguments":{}}}'
```

Typical flow is `list_windows` then `get_window_properties` (gives the root element handle) then `get_element_tree` (start with `maxElements` around 50) then `query_element_descendants` / `find_elements_by_id` / `get_element_properties` to drill in, and `click_element`, `set_element_value`, `dispatch_key_event`, `drag_element` to interact. Window handles and element handles are separate and not interchangeable. Note that `take_screenshot` currently returns a blank image for the main app window (custom GL/MPV rendering), so verify through the element tree and properties rather than screenshots.

### Create previews with mock data
When adding or changing a UI component, also create (or update) a preview in `ui/previews/` named `<component>-preview.slint`. A preview is a standalone `Window` component that imports the component and renders it with hardcoded mock data covering its main states (empty, loading, error, long text, etc.), see `ui/previews/video-card-preview.slint` for the pattern. Previews must not depend on Rust callbacks or live YouTube data, so the component's behaviour can be confirmed in isolation through the Slint MCP server.

## Checks before committing and pushing to dev

For code changes going to `dev`, a successful build from `make dev` is sufficient local build validation. Reuse an existing successful development build of the current changes rather than rebuilding just to commit or push. Also run these quick checks locally:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
```

Do not run a full release build or `cargo test --release --locked` as a routine pre-commit or pre-push check. Leave release tests to the GitHub CI workflow at `.github/workflows/ci.yml`, which also runs Clippy on pushes to `dev`. Use the GitHub CI results to identify and track any remaining failures. Only run release checks locally when the user specifically requests them.

If the local development build, formatting, or Clippy checks fail, fix the issue and re-run the affected check before committing. Documentation-only changes do not require a build or Clippy run.
