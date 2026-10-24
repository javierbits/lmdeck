# lmdeck: handover

**Repo:** https://github.com/jvr0x/lmdeck. It is a **private** repo owned by `jvr0x` (Javier).

**State (2026-10-23):**
- `main`: `cacce29` (wrap-only recipes, LMDeploy, TOML only) on top of `80fe886`, then three test-hardening commits (`322532c`, `3295fc8`, `bcb7136`), a doc update (`5be1e3c`), and the upstream update indicator for wrapped recipes (the commit after `5be1e3c`).
- CI was green on ubuntu, macOS and MSRV 1.85 at `bcb7136`; see `gh run list` for the update-indicator commit.
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
  - **Upstream update checks** (`src/updates.rs`): for cloned checkouts, `git ls-remote --symref` (timeout via a killed process group, `GIT_TERMINAL_PROMPT=0`, ssh `BatchMode`/`ConnectTimeout`, http low-speed limit) compares the checkout's HEAD to the branch head. States: `current`, `behind` (branch ref moved → `⬆ update`, `lmdeck pull`), `pinned_newer` (tag/sha pin and the default branch, or `[source] branch`, moved → `⬆ newer`, `lmdeck pull --latest` rewrites `ref` in the deck recipe), `not_cloned` (never checked), `unknown` (offline/gone; never an error), `disabled`. Cache `~/.cache/lmdeck/update-check.json`, `[updates] check/ttl_hours (6)/timeout_secs (5)`, `LMDECK_NO_UPDATE_CHECK=1`; errors are re-tried after 15 min. CLI listings re-check stale entries (3 s timeout); TUI and `serve` read the cache only and refresh in a background thread. Status is recomputed from the local HEAD each read, so it clears right after a pull. Shown in `list`, `recipe list/show` (+ JSON `update`), `lmdeck outdated [--fetch] [--json]` (alias `updates`), the TUI (badge, details line, `u` key) and the web cards (badge, `⬆ Update` button posting `{"latest":true}` for pins).
  - Built-in `ornith-1.5-122b-tensorfold` wraps MiaAI-Lab/Ornith-1.5-122B-TensorFold @ `0c8fd6a` (detached, container `ornith-122b-tf`, port 8888, `./stop.sh`, `docker logs -f ornith-122b-tf`, ready_timeout 10800, no `[model]`).
  - `examples/ornith-122b-awq-lmdeploy.toml`: LMDeploy-in-Docker example with a clearly marked **placeholder** model repo (`<your-org>/…`). Not a built-in.
  - The health check polls `/v1/models`.
  - Servers are detached processes. Stopping sends SIGTERM to the process group, then SIGKILL after 20 s. Docker servers stop with `docker stop`, LM Studio with `lms unload`.
- **CLI commands:**
  - `list [-l|-d|-a] [--json]`
  - `use`/`on`/`start` and `switch`, both with `--dry-run` and `--force`
  - `outdated [names] [--fetch] [--json]`
  - `stop [--all]`, `logs`, `pull [--latest]` (via the `hf` CLI, `lms get`, or `tensorfold pull`; updates the checkout for `[source]` recipes)
  - `add <repo>` (wrap a start.sh/stop.sh repo)
  - `init`
  - `recipe search|add|list|show|run|remove`
  - `doctor [--json]`
  - `host add|list|remove`
  - `theme list|set|show|sync-omarchy|reset`
  - `tui`, `serve`
- **Hardware and doctor** (`src/hardware.rs`, `src/compat.rs`): detects GB10, Apple Silicon, NVIDIA, or CPU-only, plus memory, compute capability, and installed engines. Produces a fit table for each recipe.
- **TUI** (`src/tui/`, ratatui + crossterm): runs on a bare `lmdeck` in a TTY. `--plain` gives the old table output.
  - Tabs: Deck, Hosts, Topology, Machine, Sources.
  - Keys: `j/k`, Enter (start/stop), `s` switch, `o` run on another host, `x` stop, `p` pull, `l` logs, `/` search, `f` fits, `a` all, `t` cycle theme, `T` theme picker, `?` help, `q` quit.
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
- **Multi-host** (`src/ops.rs`, `src/placement.rs`, `src/cluster.rs`, `src/weights.rs`, `src/fabric.rs`, `src/topology.rs`; user doc `docs/MULTIHOST.md`):
  - Remote control: `use|switch|stop|logs --on HOST`, TUI Hosts tab (`←→` model, `↵`/`s`/`x`/`l`), web host cards (`≡`/`■`, run here). Proxied through `/api/hosts/<h>/…` with per-host tokens (`token`, `token_env`, `LMDECK_HOST_TOKEN_<NAME>`). Serve token: `--token`, `LMDECK_TOKEN`, `[serve] token_env`. Non-loopback bind: every `/api` but health needs the token, reads included. Unauthenticated mutation from a non-loopback peer: 403. `[serve] accept_recipes` gates `POST /api/run` (off by default).
  - Weights: each serve indexes its HF cache + models dir (`/api/weights`); the controller merges (60 s cache). `lmdeck weights`, "⛁ weights on" in list/cards/details, `⛁` in the TUI dl column.
  - Run-on (`--on`, TUI `o`, web dropdown): the fit check uses the run host; load estimate from size / link speed. **No copy mode exists.** Weights elsewhere → Mia's `WORKER_WEIGHTS=nfs`: read-only `/etc/exports` line + `sudo -n exportfs -ra` only with `[weights] allow_export = true` (else printed), `docker volume` type nfs, `test -f`/`find -L` preflight, mount at `/root/.cache/huggingface:ro`, `HF_HUB_OFFLINE=1`. Docker recipes with HF-cache weights only. No CX7 link → refused unless `--allow-slow-link` (loud warning).
  - Multi-node `[[nodes]]` recipes: ordered start with readiness gates, rollback on failure, reverse stop, aggregate + per-node status, `logs --node`. Placeholders `{head_addr} {node_addr} {ifname} {hca} {gid_index} {weights_mount} …` mirror lmswitch `vllm-dual` (NCCL ifname/HCA/GID, compile caches off NFS). `examples/vllm-ray-2spark.toml` is marked UNTESTED.
  - CX7 detection (`/api/fabric`, no sudo): `ip -j`, `/sys/class/net`, mlx5 driver, RDMA devices, RoCE v2 GID index; L2 segment graph (cable vs synthetic switch from subnet membership / LLDP), LAN/Tailscale fallback, multi-GPU + hostname/alias/model, dot-art icons, `link_addr` and `[[link]]` overrides. Views: `lmdeck hosts topology [--json]`, TUI tab 3, web SVG.
- **Recipe index + Browse** (`index.toml`, `src/index.rs`, `src/browse.rs`, `src/tui/browseview.rs`):
  - `index.toml` at the repo root: two real MiaAI-Lab TensorFold entries (Ornith-1.5-122B on one Spark, GLM-5.3-Flash on two). Speeds and sizes are quoted from each repo's README, with a `note`; lmdeck never measured them.
  - Fetch: `[index] url` (default this repo's `main`; `LMDECK_INDEX_URL` overrides; `file://` works), cached at `~/.cache/lmdeck/index.toml`, TTL `ttl_hours` (6), `timeout_secs` (5). GitHub URLs use the contents API with `GITHUB_TOKEN`/`GH_TOKEN`/`gh auth token` (private repo works), else the raw URL; other URLs only get `[index] token_env`. Never prompts. Every failure keeps the cache with a hint. `LMDECK_NO_GH=1` skips `gh auth token`.
  - `lmdeck browse [words] [-t tag] [--sort name|sparks|speed] [--fits] [--refresh|--offline] [--json]`; `lmdeck add <name>` resolves through the index (native recipe link, else wraps the repo) with "did you mean".
  - TUI tab 6 (Browse): list (state ●◆○, ⬆ update badge, engine, ×sparks, speed, fit), details, index panel; `↵`/`i` install (subprocess `lmdeck add`, no prompts), `/` search with `tag:x`/`#x`, `S` sort, `f` fits, `r` refresh.
  - Web: a **Browse** button next to Deck/All (`?scope=browse&q=…&sort=speed`): tag chips, sort select, fits chip, cards with Install / README / copy command. API: `GET /api/index`, `POST /api/index/refresh`, `POST /api/add/{name}` (same loopback/token guard as other POSTs).
  - Fit for an index entry: sparks needed vs machines (this one + hosts.toml), the built-in recipe's compat report if it ships with lmdeck, `size_gb` vs memory.
- **Themes** (`src/theme/`, `scripts/import-omarchy-themes.py`):
  - 24 themes: `omp` (the default), the 22 Omarchy themes Javier listed, and Starship.
  - Colours are taken from `omacom/omarchy` `themes/<slug>/colors.toml` at commit 967bbdd. The repo moved there from basecamp/omarchy.
  - Contrast is tested. The theme is saved in `~/.config/lmdeck/config.toml`. `--theme` and `LMDECK_THEME` override it.
  - Omarchy's MIT licence is in `licenses/omarchy-MIT.txt`.
- **Tests:** 192 (154 unit, 18 `tests/cli.rs`, 6 `tests/index.rs` against a fake HTTP server, 5 `tests/multihost.rs` driving real `lmdeck serve` processes, 9 `tests/web.rs`). Index tests: fetch + cache + TTL, fallback to the cache with a hint (500, newer schema, offline, unreachable), the GitHub contents API + Bearer header, `token_env` (GITHUB_TOKEN not leaked to other hosts), browse filter/sort/fit, `add <name>` resolution and "did you mean"; the web test lists the index and installs by name. Test servers get `LMDECK_NO_GH=1` and a dead `LMDECK_INDEX_URL`, so nothing reaches GitHub. Update checks use local bare repos as remotes: branch and pinned recipes going behind/newer, `pull`/`pull --latest` clearing it, a removed remote → unknown, TTL 0, the env switch, the TUI badge and `u`, and the web badge + pull. No network or GPU. New ones cover: wrapper parse; detached start with a stub `start.sh` (backgrounds `python3 -m http.server`) reaching ready and stopping via `stop.sh`; detached failure + log follower; source clone + update + pin from a local git repo; `lmdeck add` on a local fixture, end-to-end (add → dry-run → detached use → stop → pull); lmdeploy native/docker dry-run and the example; YAML rejected.
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
- Update checks: tested only against local bare repos. Real GitHub over HTTPS/SSH, the timeout on a hung network, and private repos that need credentials (they should show `unknown`, never prompt) are unverified. The web badge/button was checked through the API and a JS syntax check, not visually.

- Recipe index: the real private `index.toml` fetch was verified once from this VM with a `gh` token (and the 404 + hint without one). Installing the two real entries from Browse was not run here (it would clone the MiaAI-Lab repos; `add` of Ornith was checked earlier via `add <repo>`). The web Browse view was checked in headless Chrome (screenshot), the TUI tab only in the simulated terminal.
- Multi-host: tested on one VM only (three `lmdeck serve` homes on loopback, hand-written `[[link]]` entries, python stub servers, real small downloaded weights). Never run: real CX7 cabling or detection on a Spark (the fixtures are constructed, not captured), NFS exports, `exportfs`, docker NFS volumes and the preflight, Ray/vLLM across nodes, `examples/vllm-ray-2spark.toml`, the load-time estimate against reality, and the non-loopback 403 path (no second interface here).

## Suggested next steps (rough priority)
1. **Real-hardware pass on a Spark:**
   - `cargo install --git https://github.com/jvr0x/lmdeck`
   - `lmdeck doctor`
   - `lmdeck host list` and `LMDECK_DEBUG=1 lmdeck serve`
   - Start one llama.cpp recipe and one vLLM recipe.
   - Fix whatever doesn't match reality.
2. **Same pass on the Mac:** MLX, LM Studio, TensorFold.
3. **Multi-host on real Sparks:** check CX7 detection against `lmdeck hosts topology`, one run-on over NFS (`--on`, with `allow_export` off first), then the UNTESTED vLLM+Ray example.
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
