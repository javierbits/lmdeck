# lmdeck: handover

**Repo:** https://github.com/jvr0x/lmdeck. It is a **private** repo owned by `jvr0x` (Javier).

**State (2026-10-23):**
- `main`: `cacce29` (wrap-only recipes, LMDeploy, TOML only) on top of `80fe886`, then three test-hardening commits (`322532c`, `3295fc8`, `bcb7136`) and this doc update.
- CI was green on ubuntu, macOS and MSRV 1.85 at `bcb7136`.
- macOS CI note: the runners now have Homebrew Python 3.15, and a fresh `python3 -m http.server` there has taken ~40 s to answer. Tests that use it have long ready timeouts, and `free_port()` never hands out the same port twice.
- Crate `lmdeck` v0.1.0, Rust edition with `rust-version = "1.85"`, about 12.5k lines of Rust.
- Nothing has been published to crates.io, PyPI, or npm.

## What lmdeck is
lmdeck is a Rust CLI, terminal UI, and web dashboard for running local LLMs from **recipes**. A recipe is a small TOML file (TOML only; YAML support was removed) that names:
- the model (a Hugging Face repo and revision, or a local path)
- the quant (format and bits)
- the engine and its args
- the hardware it needs

lmdeck checks whether the current machine fits a recipe, builds the exact launch command, starts and stops servers, and shows their status and logs.

It is the successor to Javier's older Python tool **`jvr0x/lmswitch`**, which is public. **Do not modify lmswitch.** lmdeck used to import lmswitch YAML recipes; that was removed on Javier's call ("get rid of yaml, let's just do the toml parser").

### Design decision: wrap-only
lmdeck **never applies patches or builds engines**. Repos that need a patched engine (e.g. Mia's Ornith/TensorFold repo) are *wrapped*: lmdeck clones the repo, runs its own `start.sh`/`stop.sh` with settings as env vars, and watches/stops the server. A `[patches]` / `[engine.scripts]` / `[model.draft]` design was drafted and dropped; don't reintroduce it.

### Owner context
- Javier owns **3 NVIDIA DGX Sparks** (GB10, 128 GB unified memory, arm64 Linux) and a Mac mini (Apple Silicon, macOS 26).
- He uses EXL3 (exllamav3 / TabbyAPI), vLLM, llama.cpp, llama-swap, LM Studio, and TensorFold.
- He cares a lot about looks. He loved the omp-style TUI and the Omarchy themes.
- He wants code pushed to GitHub, not zip files.

## Features done
- **Recipes** (`src/recipe.rs`, `src/catalog.rs`): TOML schema with `[model]`, `[quant]`, `[engine]`, `[hardware]` and (custom only) `[source]` sections. Only `[engine]` is required; `[model]` is optional for `custom`. The file name is the recipe name. `.yaml`/`.yml` files are rejected with a clear error (`YAML_UNSUPPORTED`).
  - Placeholders: `{name} {port} {ctx} {model} {models_dir} {host} {repo} {revision}`, plus `{dir}` and `{container}` for custom; they also apply in `env`, `stop`, `logs`.
  - Lookup order: the user deck in `~/.config/lmdeck/recipes`, then built-ins in `recipes/`, then sources.
- **Sources** (`src/sources.rs`): `lmdeck source add|list|update|remove`.
  - Supported forms: `github:o/r[/subdir][@ref]`, `hf:o/r`, `hf:datasets/o/r`, a local folder.
  - Fetched over public HTTPS only. Mirrored to `~/.cache/lmdeck/sources/<name>/`.
  - Only `.toml` files are recipes. There is **no default source** any more: `jvr0x/ai-models` is all lmswitch YAML. Existing configs that still list it simply get 0 recipes from it.
- **Engines** (`src/engine.rs`, `src/runner.rs`): llama.cpp, vLLM (native or Docker), SGLang, LMDeploy (native or Docker; `lmdeploy serve api_server <model> --model-name --server-name --server-port --session-len [--revision]`, port 23333, aliases `lm-deploy`/`turbomind`, override `engines.lmdeploy`), TabbyAPI (EXL3), LM Studio (`lms`), MLX, TensorFold ([ashhart/TensorFold](https://github.com/ashhart/TensorFold)), and `custom`.
- **Wrapped recipes** (`custom` + `src/checkout.rs`, `src/wrap.rs`, `src/runner.rs`):
  - `[source] repo/ref` is cloned to `~/.cache/lmdeck/checkouts/<recipe>/` on first `use` (TUI/web start too), fetched + re-checked-out on `pull`. Refuses to update over tracked local changes; untracked files (`.env`) survive. `engine.dir` defaults to the checkout; a relative `dir` resolves inside it.
  - `engine.stop` runs with the start command's cwd and env; the run record stores both.
  - `detached = true`: start.sh exiting 0 is not a crash. A small `sh` wrapper writes the exit code to `run/<name>.exit`, then (if `logs` is set) `exec`s the log follower into the same log file, so TUI/web log views work unchanged. Liveness = `docker inspect` of `engine.container` if set, else exit 0 + port probe. Stop = stop script → `docker stop -t 20` (if still running) → kill the process group.
  - Args are not appended to custom commands unless given.
  - `lmdeck add <owner/repo | url | dir> [--name] [--ref] [--force]`: needs `start.sh` + `stop.sh` at the root, reads `VAR=${VAR:-default}` lines (root `*.sh`, `scripts/*.sh`, `.env.example`) for port/host/container, pins the cloned commit, writes TODOs for what it can't tell.
  - Built-in `ornith-1.5-122b-tensorfold` wraps MiaAI-Lab/Ornith-1.5-122B-TensorFold @ `0c8fd6a` (detached, container `ornith-122b-tf`, port 8888, `./stop.sh`, `docker logs -f ornith-122b-tf`, ready_timeout 10800, no `[model]`).
  - `examples/ornith-122b-awq-lmdeploy.toml`: LMDeploy-in-Docker example with a clearly marked **placeholder** model repo (`<your-org>/…`). Not a built-in.
  - The health check polls `/v1/models`.
  - Servers are detached processes. Stopping sends SIGTERM to the process group, then SIGKILL after 20 s. Docker servers stop with `docker stop`, LM Studio with `lms unload`.
- **CLI commands:**
  - `list [-l|-d|-a] [--json]`
  - `use`/`on`/`start` and `switch`, both with `--dry-run` and `--force`
  - `stop [--all]`, `logs`, `pull` (via the `hf` CLI, `lms get`, or `tensorfold pull`; updates the checkout for `[source]` recipes)
  - `add <repo>` (wrap a start.sh/stop.sh repo)
  - `init`
  - `recipe search|add|list|show|run|remove`
  - `doctor [--json]`
  - `host add|list|remove`
  - `theme list|set|show|sync-omarchy|reset`
  - `tui`, `serve`
- **Hardware and doctor** (`src/hardware.rs`, `src/compat.rs`): detects GB10, Apple Silicon, NVIDIA, or CPU-only, plus memory, compute capability, and installed engines. Produces a fit table for each recipe.
- **TUI** (`src/tui/`, ratatui + crossterm): runs on a bare `lmdeck` in a TTY. `--plain` gives the old table output.
  - Tabs: Deck, Hosts, Machine, Sources.
  - Keys: `j/k`, Enter (start/stop), `s` switch, `x` stop, `p` pull, `l` logs, `/` search, `f` fits, `a` all, `t` cycle theme, `T` theme picker, `?` help, `q` quit.
- **Web dashboard** (`src/web.rs`, `src/web/`): `lmdeck serve [--host 127.0.0.1] [-p 7979] [--token]`.
  - A single page embedded in the binary.
  - JSON API: `/api/snapshot|recipes|status|doctor|sources|logs|telemetry|hosts|themes`, plus POST `start|switch|stop|pull|theme`.
  - Security: POST requests need the header `X-Lmdeck: 1`. On localhost the Host header is checked. Binding to anything other than localhost without a token auto-generates one and prints a warning.
- **Telemetry and hosts** (`src/telemetry.rs`, `src/hosts.rs`):
  - Sampled every 2 s, with about 6 minutes of history.
  - Sources: `/proc/meminfo` (GB10 unified memory), `nvidia-smi` (temp, power, util, clocks, with N/A handled), `/proc/stat`, the spbm hwmon driver ([antheas/spark_hwmon](https://github.com/antheas/spark_hwmon)) or k10temp/coretemp/thermal zones, per-model RSS, and tok/s from engine `/metrics`.
  - On macOS it uses `vm_stat` and `sysctl`. `powermetrics` is used only with `LMDECK_POWERMETRICS=1` and `sudo -n`.
  - `hosts.toml` is stored with mode 0600. Polling is parallel with a 1.5 s timeout and Bearer tokens. Offline hosts are shown greyed out with their last-seen time.
  - `LMDECK_DEMO=1` turns on clearly labelled synthetic data, used for screenshots.
- **Themes** (`src/theme/`, `scripts/import-omarchy-themes.py`):
  - 24 themes: `omp` (the default), the 22 Omarchy themes Javier listed, and Starship.
  - Colours are taken from `omacom/omarchy` `themes/<slug>/colors.toml` at commit 967bbdd. The repo moved there from basecamp/omarchy.
  - Contrast is tested. The theme is saved in `~/.config/lmdeck/config.toml`. `--theme` and `LMDECK_THEME` override it.
  - Omarchy's MIT licence is in `licenses/omarchy-MIT.txt`.
- **Tests:** 133 (112 unit, 14 `tests/cli.rs`, 7 `tests/web.rs`). No network or GPU. New ones cover: wrapper parse; detached start with a stub `start.sh` (backgrounds `python3 -m http.server`) reaching ready and stopping via `stop.sh`; detached failure + log follower; source clone + update + pin from a local git repo; `lmdeck add` on a local fixture, end-to-end (add → dry-run → detached use → stop → pull); lmdeploy native/docker dry-run and the example; YAML rejected.
- **Docs:** the README covers install, quick start, TUI, themes, the web dashboard, hosts and telemetry, commands, sources, the recipe format, engines, TensorFold, wrapped recipes, and files. This file is also kept in the repo as `docs/HANDOVER.md`. Screenshots are in `docs/screenshots/`.

## Files and dirs at runtime
- Config: `~/.config/lmdeck/` holds `config.toml`, `recipes/`, `hosts.toml`, and sources.
- Cache: `~/.cache/lmdeck/`.
- State: `~/.local/state/lmdeck/` holds running servers and logs.
- `LMDECK_HOME` puts all three under one root.

## Not verified yet
Development happened on a CPU-only Linux VM. Nothing has been tested on a real Spark or the Mac.
- No real engine has ever been launched. Only the generated commands were checked.
  - Most uncertain: TabbyAPI CLI flags, whether `lms load` and `lms get` accept HF repo ids, Docker `--gpus all` on a Spark, and the memory numbers and parsers in the Qwen3.6 NVFP4 vLLM recipe.
- The GB10 telemetry parsers are built from documented formats and fixtures: `nvidia-smi` `[N/A]` fields and the spbm labels `sys_total`, `dc_input`, `cpu_*_clu*`. Engine `/metrics` names come from the docs and haven't been checked against live servers.
- TensorFold: serving on Metal or GB10, EXL3 on a single Spark, the Homebrew formula name, and how it behaves on SIGTERM.
- Wrapped recipes: verified only with stub scripts and a stub HTTP server, plus `lmdeck add MiaAI-Lab/Ornith-1.5-122B-TensorFold` against the real GitHub repo (clone + detection + dry-run; it detects port 8888 and container `ornith-122b-tf`). Mia's start.sh has **never been run** by lmdeck: no Docker/GPU here. The `docker inspect` liveness and `docker logs -f` follower paths are untested against real Docker.
- LMDeploy: only dry-run commands were checked. Assumed that `openmmlab/lmdeploy` images have no serving entrypoint (lmdeck puts `lmdeploy serve api_server` after the image); unverified. No AWQ Ornith build has been checked to exist.
- The TUI has only been driven in a simulated terminal, never a real one.

## Suggested next steps (rough priority)
1. **Real-hardware pass on a Spark:**
   - `cargo install --git https://github.com/jvr0x/lmdeck`
   - `lmdeck doctor`
   - `lmdeck host list` and `LMDECK_DEBUG=1 lmdeck serve`
   - Start one llama.cpp recipe and one vLLM recipe.
   - Fix whatever doesn't match reality.
2. **Same pass on the Mac:** MLX, LM Studio, TensorFold.
3. **Remote control from the Hosts view:** proxy start, stop, and switch to remote `lmdeck serve` instances. The view is watch-only today.
4. **Multi-node recipes** (`nodes = 2`) across two Sparks. Currently they are listed but refused.
4b. **Run the Ornith wrapper on a Spark** (`lmdeck use ornith-1.5-122b-tensorfold`), check the detached/container/logs path, and tune `ready_timeout`.
4c. **Convert `jvr0x/ai-models` to TOML** if Javier wants a default source again (that repo needs his OK; don't touch lmswitch).
5. **llama-swap config export** (`lmdeck export llama-swap`).
6. **Smaller fixes:**
   - Docker recipes have no RSS.
   - MLX and LM Studio have no tok/s.
   - Tiny local models show "0M" in the size column.
   - The older screenshots (`tui-deck`, `tui-machine`, `web-dashboard`) still show the 3-tab header.
7. **Launch prep:** Javier plans a public launch of lmdeck and is growing his X audience (@jvr0x) for it. The repo is private until he decides. Don't make it public, publish to crates.io, or post anything without his go-ahead.

## Working rules
- Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` before every push. Keep MSRV 1.85 building.
- Commit and push in small steps, then confirm CI with `gh run list`.
- Never put secrets in the repo. Tokens like `HF_TOKEN` are passed by name only and never written to disk.
- No fake numbers in real code paths. Mock data only goes behind `LMDECK_DEMO=1`, labelled as demo data.
- Don't invent theme palettes, HF repo ids, or engine flags. Verify against the real source.
