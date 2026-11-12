# AGENTS.md: lmdeck

This file tells an agent how lmdeck works and how to author, wrap, and wire
recipes. `README.md` is the user manual; this is the implementation-aware
playbook. If you change the recipe format or a launch path, update both.

Verified against lmdeck 0.1.3, schema 1, 2026-11-12.

## What lmdeck is

lmdeck is one Rust binary that launches local LLM servers from declarative TOML
recipes. A recipe names the model, quantization, engine, hardware needs, and
(optionally) several machines. lmdeck builds the engine command line, fills
machine-specific placeholders, pulls weights, starts the process or container,
waits for `/v1/models`, and records status/logs/telemetry.

The same catalog powers:

- the `lmdeck` CLI (`list`, `use`, `switch`, `stop`, `pull`, `doctor`, ...);
- the full-screen TUI (bare `lmdeck`);
- the web dashboard and JSON API (`lmdeck serve`, default `127.0.0.1:7979`);
- agent-harness sync (`lmdeck harness`), which points OpenCode/omp/Grok/pi/hermes
  at served models.

## Mental model

```text
Catalog (deck + peers + built-ins + uploaded remote recipes + sources)
  -> resolve name
  -> parse + validate Recipe (src/recipe.rs)
  -> compatibility/fit check (src/compat.rs)
  -> plan: argv, env, cwd, docker args, placeholders (src/engine.rs)
  -> optional checkout / weight pull / NFS mount / node placement
  -> runner starts process or container, waits for readiness (src/runner.rs)
  -> run record + logs + telemetry
```

A bare recipe name resolves to the deck first, then a peer's deck. Otherwise
the origins are considered in catalog order (built-ins, uploaded remote
recipes, sources) and a duplicate name is ambiguous. `origin/name` addresses
one explicitly. Keep names unique.

### Where things live

| What | Path / file |
|---|---|
| User deck | `~/.config/lmdeck/recipes/*.toml` (`$XDG_CONFIG_HOME`) |
| Config | `~/.config/lmdeck/config.toml` |
| Hosts and tokens | `~/.config/lmdeck/hosts.toml` (mode 0600) |
| Source mirrors | `~/.cache/lmdeck/sources/<name>/` |
| Wrapped checkout | `~/.cache/lmdeck/checkouts/<recipe>/` |
| Run records / logs | `~/.local/state/lmdeck/{run,logs}/` |
| Usage statistics (opt-in) | `~/.local/state/lmdeck/usage.json` (`usage.lock` elects one recorder) |
| Model weights | HF cache (`$HF_HUB_CACHE`, `$HF_HOME/hub`, or `~/.cache/huggingface/hub`) or `models_dir` for local/path weights |
| Release workflow | `.github/workflows/release.yml` (tag `v*` -> binaries, checksums, crates.io, Homebrew tap) |
| Installer | `install.sh` (`curl \| sh`, verifies the release checksum) |
| Release runbook | `RELEASING.md` |

`LMDECK_HOME=<dir>` relocates all of the above; use it for isolated tests.

### Modules you will touch

| Module | Responsibility |
|---|---|
| `src/recipe.rs` | Recipe structs, parsing, validation, name/port rules |
| `src/recipe/nodes.rs` | `[[nodes]]` and `[cluster]` |
| `src/engine.rs` | Builds argv/env/cwd for every engine; resolves placeholders |
| `src/engine_install.rs` | Managed llama.cpp download: pinned release, SHA-256, unpacked into the lmdeck cache |
| `src/runner.rs` | Spawns, probes readiness, stops, captures logs |
| `src/catalog.rs` | Merges deck/builtin/peers/remote/sources; `add_to_deck` |
| `src/deck.rs` | Live snapshot, status, port planning |
| `src/wrap.rs` | `lmdeck add`: repo manifest vs `start.sh`/`stop.sh` wrapper |
| `src/compat.rs` | Fit checks against hardware and installed engines |
| `src/vars.rs` | Machine placeholders (`{hf_home}`, `{llama_server}`, ...) |
| `src/install.rs` | `lmdeck add` and `lmdeck recipe add` (index lookup, repo wrap, manifest, file/URL); shared by the CLI and the dashboard |
| `src/web.rs`, `src/web/index.html` | `lmdeck serve`: the JSON API and the embedded dashboard (Deck, Library, Stats, Weights, Sources, Hosts, Topology, Doctor). A route that writes config or the deck reloads the running deck |
| `src/fabric.rs`, `mesh.rs`, `placement.rs`, `pick.rs` | CX7 fabric, peers, `host = "auto"` placement |
| `src/hosts.rs`, `src/remote.rs` | Remote `lmdeck serve` machines and run-on |
| `src/harness.rs` | Agent-harness config sync |
| `src/usage.rs` | Opt-in per-machine usage statistics from engine counters; the Stats tab merges hosts via `GET /api/usage` |

## Recipe format

Recipes are TOML only. YAML is rejected everywhere: deck files, `recipe add`,
and source mirrors. Every recipe should start with `schema = 1` (missing means
1, but new files write it). `min_lmdeck = "x.y.z"` is only for a file that
needs a newer lmdeck; do not invent version fields.

A recipe's identifier is `name`, or the file stem when `name` is absent.
Names use `[A-Za-z0-9._-]`, at most 128 chars, and may not start with `.`.
Keep `name` and the file stem equal unless you have a reason not to.

Unknown keys are ignored by the deserializer, so a typo can silently do
nothing. `lmdeck use <name> --dry-run` shows the effect of the fields lmdeck
actually read; use it instead of trusting a field name by eye.

### Top level

```toml
schema       = 1
name         = "qwen3-4b-gguf"        # optional; defaults to the file stem
display_name = "Qwen3 4B (GGUF Q4_K_M)"
description  = "Qwen3 4B on llama.cpp. Small smoke-test model."
tags         = ["qwen3", "small", "chat", "dgx-spark"]

[model]     # optional only for custom recipes
[quant]     # optional but strongly recommended
[engine]    # required
[hardware]  # optional; controls fit checks
[harness]   # optional; chat model by default
[source]    # custom only; checkout to run scripts from
[cluster]   # optional; multi-node start order
[[nodes]]   # multi-node; exactly one head
```

### `[model]`: where the weights are

| Field | Meaning |
|---|---|
| `source` | `"hf"` (default) or `"local"` |
| `repo` | Hugging Face id, exactly `org/name` (required for `hf`) |
| `revision` | branch, tag, or commit; used by `pull` and `--revision` |
| `files` | files or globs to fetch; for GGUF the first entry is loaded |
| `path` | local weights path, relative to `models_dir` or absolute (required for `local`) |
| `size_gb` | approximate size; used for memory estimates when no `min_memory_gb` |
| `home` | host holding the weights; normally omit and let lmdeck discover it |
| `pull` | `false`: the recipe's scripts fetch their own weights; lmdeck only locates the host |
| `nfs_volume` | internal, written by lmdeck when weights are mounted remotely; never hand-write |

Rules:

- Non-custom recipes need `model.repo` (unless `model.path` for a local
  checkpoint). Custom recipes may omit `[model]` entirely.
- `llama.cpp` needs `model.files = ["<file>.gguf", ...]` or `model.path`.
- `TabbyAPI` and `TensorFold` only serve weights already on disk:
  run `lmdeck pull <recipe>` first. `TabbyAPI` has no streaming path.
- `vLLM`, SGLang, MLX, and llama.cpp can use a repo id directly; llama.cpp
  falls back to its HF downloader when the file is absent.
- `pull = false` is for wrapped recipes whose scripts download or generate
  their own weights. It makes lmdeck find the host that has them, not pull.
- `model.home` / `nfs_volume` are placement concerns; use `--on`/`auto`
  placement instead of hard-coding a host in a normal recipe.

### `[quant]`: what the checkpoint is

```toml
[quant]
format = "exl3"     # gguf, exl3, exl2, nvfp4, mxfp4, fp8, awq, gptq, mlx, bf16, ...
bits   = 4.0        # optional
label  = "4.0bpw"   # optional free-form label
```

Set this accurately. It is shown in the UI, used by search, and TensorFold's
fit check rejects `gguf`, `exl2`, `awq`, and `gptq` checkpoints.

### `[engine]`: what to launch

| Field | Meaning |
|---|---|
| `name` | `llama.cpp`, `vllm`, `sglang`, `lmdeploy`, `tabbyapi`, `lmstudio`, `mlx`, `tensorfold`, or `custom` |
| `port` | server port; omit to let lmdeck plan one from `[serve] port_start` |
| `ctx` | context length |
| `args` | extra flags, appended last; placeholders allowed |
| `env` | environment for the process/container; placeholders allowed |
| `image` | run vLLM/SGLang/LMDeploy in Docker with this image |
| `entrypoint` | Docker `--entrypoint` override, e.g. `["vllm", "serve"]` |
| `volumes` | extra Docker `-v host:container[:ro]` mounts |
| `bin` | binary override for this recipe |
| `dir` | working directory; for a wrapped recipe relative to its checkout, for `tabbyapi` the TabbyAPI checkout |
| `command` | custom only: argv to run |
| `command_by_fabric` | custom only: argv per detected fabric shape |
| `stop` | custom only: argv that stops the server |
| `detached` | custom only: command exits 0 once the server is up |
| `container` | custom only: Docker container for liveness / fallback stop |
| `logs` | custom only, with `detached`: argv that streams logs |
| `ready_timeout` | seconds to wait for `/v1/models`; default 900 |

Engine defaults and what lmdeck adds (these ports are the engine defaults when
a recipe is considered alone; deck and source recipes without an explicit
`[engine] port` are counted from `[serve] port_start`, so set a port explicitly
for wrapped scripts that must bind a known one):

| Engine | Default port | Model argument | Flags lmdeck adds unless already in `args` |
|---|---:|---|---|
| `llama.cpp` | 8080 | `--model <path or --hf-repo/--hf-file>` | `--alias`, `--host`, `--port`, `--ctx-size`, `--n-gpu-layers 99`, `--metrics` |
| `vllm` | 8000 | positional repo/path | `--served-model-name`, `--host`, `--port`, `--max-model-len`, `--revision` |
| `sglang` | 30000 | `--model-path` | `--served-model-name`, `--host`, `--port`, `--context-length`, `--revision` |
| `lmdeploy` | 23333 | positional repo/path | `--model-name`, `--server-name`, `--server-port`, `--session-len`, `--revision` |
| `tabbyapi` | 5000 | `--model-dir` + `--model-name` | `--host`, `--port`, `--backend exllamav3`, `--max-seq-len` |
| `lmstudio` | 1234 | `lms load <model> --identifier <name>` | `lms server start`, `--context-length`; stop is `lms unload` |
| `mlx` | 8080 | `--model` | `--host`, `--port` |
| `tensorfold` | 8080 | positional repo/checkpoint | `--name`, `--host`, `--port`, `--context` |
| `custom` | 8080 | none; `command` is verbatim | nothing; `args` are appended only if present |

Docker notes:

- `image` is for vLLM, SGLang, and LMDeploy native engines. lmdeck creates
  `lmdeck-<recipe>` with `--gpus all --network host --ipc host` and mounts the
  HF home; it forwards `HF_TOKEN` by name only. Use `volumes` for extra data.
- For a multi-node or custom recipe, `image` is just the `{image}` placeholder
  value; your `docker run` command must use it.
- Images must already exist on every machine that runs them.

Custom-only fields are rejected on native engines:
`stop`, `command_by_fabric`, `detached`, `container`, `logs`, and `[source]`.

### `[hardware]`: fit checks

All fields optional:

```toml
[hardware]
min_memory_gb = 24              # per machine; unified memory, VRAM, or RAM
accelerator   = ["nvidia"]      # nvidia | apple | amd | cpu
os            = ["linux"]       # linux | macos
arch          = ["aarch64"]     # aarch64 | x86_64
min_compute_capability = "10.0" # e.g. NVFP4 needs Blackwell; GB10 is 12.1
nodes         = 1               # must match [[nodes]] when both are set
hosts         = 4               # wrapped scripts span N Sparks; fleet fit and memory share
```

Memory estimate: `min_memory_gb` if set; otherwise the on-disk weight size
(else `size_gb`), plus 20% on one machine, or divided by the machine count for
a multi-machine recipe. `min_memory_gb` is never divided because it already
means per machine. Set `min_memory_gb` when you know the real requirement; a
recipe with neither size nor minimum is only a warning, not a useful fit.

### `[harness]`

```toml
[harness]
chat = false   # not a chat model; harness sync leaves it out
```

Default is `true`. Use `false` for embeddings, rerankers, classifiers, and
other non-chat servers. Vision detection is heuristic based on recipe name /
display name; do not rely on a field.

## The four recipe shapes

### A. Native engine recipe (no wrapper)

Use when lmdeck knows how to launch the engine. The model repo is enough.

```toml
schema = 1
display_name = "Qwen3 4B (GGUF Q4_K_M)"
description = "Qwen3 4B on llama.cpp. Tiny smoke-test model: ~2.5 GB."
tags = ["qwen3", "gguf", "small"]

[model]
repo = "unsloth/Qwen3-4B-GGUF"
files = ["Qwen3-4B-Q4_K_M.gguf"]
size_gb = 2.5

[quant]
format = "gguf"
label = "Q4_K_M"

[engine]
name = "llama.cpp"
port = 8080
ctx = 32768
args = ["--jinja"]

[hardware]
min_memory_gb = 4
```

Do not add `stop`, `detached`, `container`, `logs`, `command`, or `[source]`.
lmdeck owns the process lifecycle.

### B. Wrapped custom recipe (`start.sh` / `stop.sh`)

Use when a model repo ships its own launcher. `lmdeck add <repo>` clones it,
detects the scripts, pins the commit, and writes a deck recipe like:

```toml
schema = 1
display_name = "My model (own scripts, 1x Spark)"

[source]
repo = "https://github.com/owner/repo"
ref = "0c8fd6a0d1e2ece7557978a7d8c3d96d137eb075"  # commit pin; use "main" to follow a branch

[quant]
format = "nvfp4"
bits = 4.0

[engine]
name = "custom"
command = ["./start.sh"]
stop = ["./stop.sh"]
detached = true
container = "my-model"
logs = ["docker", "logs", "-f", "my-model"]
port = 8888
ready_timeout = 3600

[engine.env]
PORT = "{port}"
HOST = "{host}"
CONTAINER_NAME = "{container}"

[hardware]
min_memory_gb = 24
accelerator = ["nvidia"]
os = ["linux"]
```

Contract:

- `command` runs from the checkout (or `dir` inside it). `stop` runs with the
  same cwd and env. Nothing is appended to `command` except `args` if you set
  them. A hand-written `[source]` is cloned on the first use; `lmdeck add`
  has already cloned it.
- `logs` runs after `command` exits (the detached case) and streams into
  lmdeck's log file. It must not itself exit immediately if you want a live
  view.
- If `command` blocks, leave `detached = false`; lmdeck tracks the process.
- If `command` exits 0 after handing off (`docker run -d`, `nohup`, `setsid`),
  set `detached = true`. Liveness then comes from `container` when set, else
  from probing `port`. A non-zero exit is a failure, unless the container is
  still running.
- `logs` is allowed only with `detached`. `stop` is tried first, then
  `docker stop <container>`, then process termination.
- The server must answer `GET /v1/models` with 2xx. `ready_timeout` covers
  image builds and first downloads.
- `lmdeck outdated` compares the checkout's commit with upstream. A branch
  `ref` shows `update`; a pinned tag/commit shows `newer (pinned)`.
  `lmdeck pull <name>` follows the current pin; `lmdeck pull --latest <name>`
  rewrites the pin to the upstream head.
- `[engine.env]` is how lmdeck's settings reach your scripts. Map at least
  `PORT`, and `HOST` / `CONTAINER_NAME` if the scripts read them. Placeholders
  work in env values.
- `[model]` is optional. If the scripts fetch their own weights, use
  `pull = false` (or omit `[model]`) so lmdeck does not download or mount them.
- `[source].ref` can be a branch, tag, or commit. A commit/tag is a pin;
  `lmdeck pull --latest` moves it. `[source].branch` names the branch to compare
  against when `ref` is a tag/commit.

### C. A repo's root `lmdeck.toml`

This is the manifest `lmdeck add <repo>` prefers over `start.sh` detection.
It makes a model repo installable by anyone:

```sh
lmdeck add https://github.com/owner/repo
```

Rules:

- Put it at the repository root, named exactly `lmdeck.toml`.
- Install a repo manifest with `lmdeck add <repo>`, not by adding its raw
  `lmdeck.toml` URL with `lmdeck recipe add`. `recipe add` does not inject
  `[source]`, so a relative `command` would run from lmdeck's process cwd.
- It is a normal recipe, but:
  - do not write `[source]`; `lmdeck add` injects the cloned URL and pinned
    commit;
  - `name` is ignored; the deck name is `--name` or the repo directory name.
    Use `display_name` for the picker.
- **Use `engine = "custom"` for a repo-root manifest.** `lmdeck add` injects
  `[source]`, and recipe validation rejects `[source]` on native engines. A
  native-engine manifest added this way writes a recipe that fails to load
  (`lmdeck doctor` shows it). If you need to publish a native-engine recipe,
  distribute it as a normal `.toml` and add it with
  `lmdeck recipe add <raw-url>.toml`, not as a root manifest.
- `command` paths are relative to the checkout. `[engine.env]` receives
  `{port}`, `{host}`, `{container}`, etc.
- If the scripts drive several Sparks themselves (ssh/docker), set
  `[hardware] hosts = N`; do not add `[[nodes]]`. The fit check then requires
  N online Sparks, and lmdeck starts the recipe on one machine.
- If the scripts fetch their own weights, set `[model].pull = false` (or omit
  `[model]`). lmdeck then only uses `repo`/`revision` to find the host that
  holds them; it does not download or mount anything.
- If the repo has no `lmdeck.toml`, `lmdeck add` needs `start.sh` and `stop.sh`
  at the root. It cannot wrap an arbitrary launcher in a subdirectory.

Minimal repo-root manifest:

```toml
schema = 1
display_name = "My model (vLLM, 1x Spark)"

[model]
repo = "org/model"
size_gb = 24

[quant]
format = "nvfp4"

[engine]
name = "custom"
command = ["bash", "scripts/serve.sh"]
stop = ["bash", "scripts/stop.sh"]
port = 8000
detached = true
container = "my-model"
logs = ["docker", "logs", "-f", "my-model"]
ready_timeout = 3600

[engine.env]
PORT = "{port}"
HOST = "{host}"

[hardware]
min_memory_gb = 24
accelerator = ["nvidia"]
os = ["linux"]
```

### D. Multi-node recipe (`[[nodes]]` and `[cluster]`)

Use this only when lmdeck must start a different command on each machine.
Each `[[nodes]]` entry is one machine; exactly one is `role = "head"`.

```toml
[model]
repo = "org/model"
size_gb = 24

[engine]
name = "custom"
port = 8000
ready_timeout = 1800

[hardware]
nodes = 2
min_memory_gb = 24

[cluster]
order = "head-first"    # or "workers-first"; default head-first
gate_timeout = 600      # seconds per node gate; default 120

[[nodes]]
name = "head"
role = "head"
host = "local"          # local, this machine's [node] name, or a hosts.toml name
port = 8000             # head status probe; defaults to the recipe engine port
ready_port = 6379       # gate: wait for this TCP port; optional
command = ["docker", "run", "...", "{image}", "...", "--master-addr", "{master_addr}"]
env = { NCCL_SOCKET_IFNAME = "{ifname}", VLLM_HOST_IP = "{node_addr}" }
container = "my-head"
stop = ["docker", "rm", "-f", "my-head"]
logs = ["docker", "logs", "-f", "my-head"]
detached = true

[[nodes]]
name = "w1"
role = "worker"
host = "auto"           # lmdeck places the worker on the head's CX7 fabric
ready_delay = 10        # gate: still running after 10 s; default 3
command = ["docker", "run", "...", "{image}", "...", "--headless"]
env = { NCCL_SOCKET_IFNAME = "{ifname}", VLLM_HOST_IP = "{node_addr}" }
container = "my-w1"
stop = ["docker", "rm", "-f", "my-w1"]
logs = ["docker", "logs", "-f", "my-w1"]
detached = true
```

Rules and behavior:

- Node fields: required `name`, `role`, `host`, `command`; optional `env`,
  `stop`, `container`, `detached`, `logs`, `port`, `ready_port`, `ready_delay`.
  `port` is the status probe: the head defaults to the recipe's engine port, a
  worker to its `ready_port`.
- `hardware.nodes` must equal the number of `[[nodes]]` entries when set.
- The head needs a real host (`local` or a hosts.toml name); it cannot be
  `"auto"`. Workers can be `"auto"`; placement is deterministic around the
  head's fabric.
- `logs` requires `detached`; `[cluster]` requires `[[nodes]]`.
- Start order: `head-first` (Ray) or `workers-first` (vLLM/SGLang where the
  head waits for workers). Nodes start in order; each next node waits for the
  previous gate. Stop is reverse order.
- Gates: `ready_port` answers on TCP, or `ready_delay` seconds pass; the last
  node, when it is the head, must serve `/v1/models` like any recipe.
- `min_memory_gb` is per node. Without it, the weights size is divided over
  the nodes.
- `[model]` is mounted only on nodes whose command/env uses `{weights_mount}`
  or `{weights_volume}`. A node that fetches from the head needs no mount.
- With `[source]`, every node runs from its own checkout pinned to one commit.

See `examples/vllm-ray-2spark.toml` for a complete Ray example and
`recipes/deepseek-v4.1-flash-1017-4x-spark-nvfp4.toml` for a working four-node
vLLM/TP4 recipe. `docs/MULTIHOST.md` is authoritative for placement and fabric
placeholders.

## Placeholders

Placeholders are substituted in `args`, `env` values, `volumes`, `command`,
`command_by_fabric`, `stop`, `container`, and `logs`. A placeholder lmdeck
cannot fill fails the plan and `--dry-run`; it never reaches a command as
`{x}`. Braces that are not a lowercase name (shell `${VAR}`, JSON
`{"method":"mtp"}`) are left alone. If you need a literal lowercase `{name}`,
put it in a script file.

| Group | Placeholders |
|---|---|
| Recipe | `{name}`, `{port}`, `{ctx}`, `{model}`, `{models_dir}`, `{host}`, `{repo}`, `{revision}` |
| Custom engine | `{dir}` (working directory), `{container}`, `{fabric}` |
| Machine | `{user}`, `{home}`, `{hf_home}`, `{model_dir}` |
| Binaries | `{llama_server}`, `{vllm}`, `{python}`, `{lmdeploy}`, `{lms}`, `{mlx_server}`, `{tensorfold}`, `{docker}`, and `{<binary>_dir}`; `{tabbyapi_dir}` |
| Wrapped multi-Spark | `{peer_addr}` / `{peer_host}`, `{peer2_addr}` / `{peer2_host}`, ... for the peers lmdeck picks around this Spark |
| Multi-node | `{head_addr}`, `{master_addr}`, `{node_addr}`, `{ifname}`, `{hca}`, `{gid_index}`, `{rank}`, `{nodes}`, `{node}`, `{image}`, `{head_port}`, `{weights_mount}`, `{weights_volume}`, `{weights_at}` |
| Fabric | `{fabric}`, `{fabric_order}`; on a ring also `{ring_rank}`, `{ring_prev}`, `{ring_next}`, `{ring_prev_addr}`, `{ring_next_addr}`, `{ring_prev_ifname}`, `{ring_next_ifname}` |

`{model}` is the repo id, unless the recipe uses local weights, in which case
it is the path. `{model_dir}` is the directory holding the first `[model] files`
entry on that machine. `args` that already set one of lmdeck's default flags
suppress that default, including `--flag=value` forms.

`command_by_fabric` keys are exactly: `single`, `cable`, `mesh`, `ring`,
`switch`, `mixed`, `none`. `command` covers every shape not listed.

## Wrapping: how `lmdeck add <repo>` decides

1. Clone/resolve the repo to `~/.cache/lmdeck/checkouts/<name>/`.
2. If a root `lmdeck.toml` exists, parse it, inject `[source]`, install that.
   A parse error is reported; there is no fallback to scripts.
3. Otherwise require root `start.sh` and `stop.sh`.
4. Read `VAR=${VAR:-default}` lines from root `*.sh`, `scripts/*.sh`, and
   `.env.example` (first default wins):
   - port from `PORT`, `SERVER_PORT`, `API_PORT`, `HTTP_PORT`,
     `LISTEN_PORT`, else any `*_PORT`;
   - host from `HOST`, `SERVER_HOST`, `BIND_HOST`, `LISTEN_HOST`;
   - container from `CONTAINER_NAME`, `CONTAINER`, else any `*CONTAINER*`.
5. Detect `detached` from `docker run -d`, `--detach`, `compose up -d`,
   `nohup`, or `setsid` in the scripts.
6. Root `start*.sh` scripts other than `start.sh` map by name:
   `switchless` / `ring` -> `ring` and `mesh`; `roce` / `switch` -> `switch`;
   `single` / `solo` -> `single`. Unmatched scripts become TODOs.
7. Detect `[hardware] hosts = N` from "Nx DGX Sparks" in the repo name or
   README, ignoring 1.
8. Set `ready_timeout` from the largest "N GB/TB" in the README: one minute
   per GB, clamped to 1-12 hours.
9. Pin the cloned commit unless `--ref` was passed. The generated recipe has
   TODOs where detection failed; review all of them.

A repo that a built-in recipe already wraps gets that hand-tuned built-in copy
unless you pass `--name`; pass a name to force a fresh wrapper.

If you control the model repo, make wrapping clean: root `start.sh` and
`stop.sh`, `PORT="${PORT:-8888}"`, `CONTAINER_NAME="${CONTAINER_NAME:-...}"`,
honor `HOST`, use `docker run -d` only when you want `detached`, and name
fabric launchers `start-switchless.sh` / `start-roce.sh`.

`lmdeck add <name>` first resolves an index entry. If the entry has a `recipe`
and no `--ref` was passed, it installs that native recipe (the built-in copy
when the name matches); otherwise it adds the repo through the same
manifest/wrapper path. An index name that looks like a typo gets
`did you mean: ...`.

## Wiring recipes in

### 1. User deck (the common case)

Any `.toml` in `~/.config/lmdeck/recipes/` is a deck recipe. The file stem is
the name unless `name` says otherwise.

```sh
# from a local file, a raw GitHub URL, or another deck/builtin/source recipe
lmdeck recipe add ./my-recipe.toml
lmdeck recipe add https://raw.githubusercontent.com/owner/repo/main/recipes/my-model.toml
lmdeck recipe add builtin/qwen3-4b-gguf --name my-qwen
lmdeck recipe add source-name/my-model

# or just write the file yourself
$EDITOR ~/.config/lmdeck/recipes/my-model.toml
```

`recipe add` copies the recipe as TOML into the deck; `--force` overwrites.
A URL must end in `.toml`; GitHub blob URLs are converted. A custom recipe
installed this way has no injected `[source]`, so a relative `command` runs
from lmdeck's process cwd. Set `[source]`, an absolute `dir`, or absolute
commands for standalone custom recipes.

Then validate and run:

```sh
lmdeck doctor
lmdeck use my-model --dry-run
lmdeck pull my-model          # only when the recipe needs downloaded weights
lmdeck use my-model
lmdeck logs my-model
```

`--dry-run` prints fit, cwd, argv, env, placeholders, weights placement, peers,
and the planned port without starting anything. Do this before a real start.

### 2. A recipe source (a repo of recipes)

A source is a repo or local directory containing `.toml` recipes; layout does
not matter. lmdeck fetches only `.toml`, up to 500 files of 512 KiB each.
Files without an `[engine]` table are skipped; parse errors appear in
`lmdeck doctor`.

```sh
lmdeck source add github:someone/recipes
lmdeck source add github:someone/recipes/spark@v2
lmdeck source add hf:someone/lmdeck-recipes
lmdeck source add hf:datasets/someone/recipes@main
lmdeck source add ~/src/my-recipes
```

Equivalent config:

```toml
[[sources]]
name = "mine"
spec = "github:someone/recipes"
```

Use `lmdeck source update` to refresh mirrors. Recipes from sources are
read-only origins; copy one into the deck with
`lmdeck recipe add <source>/<name>` before editing it.

### 3. The public index (`index.toml`)

`lmdeck browse` and `lmdeck add <name>` read the index from `[index] url`
(default: the lmdeck repo's `index.toml`; cached, TTL 6 h). An entry is:

```toml
schema = 1

[[entry]]
name        = "my-model"                  # what `lmdeck add` takes; [A-Za-z0-9._-]
repo        = "https://github.com/owner/repo"
ref         = "v1.2.3"                    # optional pin: tag, branch, or commit
description = "One sentence for the browser."
size_gb     = 24.0                        # weights on disk, from the repo's README
recipe      = "recipes/my-model.toml"     # optional, relative to index.toml or a URL
tags        = { engine = "vllm", model = "MyModel", family = "myfamily", quant = "nvfp4", sparks = 1 }
speed       = { tok_s = 42.0, note = "repo README: one Spark, one stream, prose" }
```

Rules:

- Only real, published repos. Numbers come from the repo's own README, with
  `note` saying where; lmdeck never measures them for the index.
- `recipe` is optional. Without it, `add` uses the repo's root `lmdeck.toml`,
  then `start.sh`/`stop.sh` wrapping.
- `tags.sparks` is numeric; `engine`, `model`, `family`, `quant` are strings.
- Keep `name` unique; it is the install handle and the deck name.

### 4. Built-in recipes (shipped in the binary)

`BUILTIN_RECIPES` in `src/catalog.rs` ships the nine starter recipes;
`STARTER_RECIPES` is the first-run wizard's menu (name plus a one-line
capability note). To ship another:

1. Add `recipes/<name>.toml` with `schema = 1`.
2. Add `("<name>.toml", include_str!("../recipes/<name>.toml"))` to
   `BUILTIN_RECIPES` in `src/catalog.rs`.
3. Add it to `STARTER_RECIPES` if the wizard should offer it.
4. If the model repo is public and worth browsing, add an `index.toml` entry.
5. `cargo test` (the catalog test parses every built-in and checks unique
   names).

`recipes/` also holds the files `index.toml` points at; those are installed
with `lmdeck add` and are not compiled in. Built-ins are compile-time
`include_str!` entries: rebuild the binary after editing one. They do not take
counted ports and are hidden from the deck view when the deck has entries, but
they remain addressable by name. `lmdeck recipe add <builtin-name>` copies one
into the deck.

### 5. `examples/` recipes

Files in `examples/` are not compiled in and not shown by default. They are
documentation and templates:

```sh
lmdeck recipe add examples/vllm-ray-2spark.toml
```

If an example is untested or has a placeholder repo, say so in the file header
and README, as the existing examples do.

## Agent checklist for a new recipe

1. Choose the shape: native engine, wrapped custom, repo manifest, or
   multi-node.
2. Write the recipe with `schema = 1`, `display_name`, `description`, accurate
   `[quant]`, and `[hardware]`.
3. For custom: set `command`, `stop`, `detached`, `container`, `logs`, `port`,
   `ready_timeout`, and map `PORT`/`HOST` in `[engine.env]`.
4. For a wrapped repo: set `[source].ref` (commit pin or branch) and review
   every generated TODO.
5. For multi-node: `[hardware] nodes`, `[cluster]`, exactly one head, explicit
   commands and gates, and only mount weights on nodes that use them.
6. Validate locally:

```sh
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release

H=$(mktemp -d)
LMDECK_HOME="$H" LMDECK_NO_SYNC=1 ./target/release/lmdeck recipe add ./my-recipe.toml
LMDECK_HOME="$H" LMDECK_NO_SYNC=1 ./target/release/lmdeck doctor
LMDECK_HOME="$H" LMDECK_NO_SYNC=1 ./target/release/lmdeck use <name> --dry-run
```

7. If the recipe is actually runnable here, go further:
   `lmdeck pull <name>` then `lmdeck use <name>` and `lmdeck logs <name>`.
   A dry run alone does not prove the engine starts.
8. Update `README.md` when the recipe format, a built-in, or an example
   changes. Keep this file in sync when validation, wiring, or launch behavior
   changes.

## Validation errors and common mistakes

| Symptom / error | Cause and fix |
|---|---|
| `recipes must be .toml files` / YAML error | Rewrite as TOML. YAML is not supported. |
| `engine "custom" needs engine.command` | Add `command` (unless the recipe has `[[nodes]]`). |
| `stop is only for engine "custom"` | Remove custom-only fields from a native engine recipe. |
| `model.source = "hf" needs model.repo` | Add `[model] repo = "org/name"` or make the recipe `custom`. |
| `model.repo ... should look like 'org/name'` | Exactly two slash-separated parts. |
| `llama.cpp recipes need model.files = ["<file>.gguf", ...] or model.path` | Name the GGUF to load; the first entry is loaded. |
| `[source] is only for engine "custom"` | A root manifest with a native engine cannot be installed by `lmdeck add`; publish it as a normal recipe file instead. |
| `no start.sh or stop.sh` from `lmdeck add` | Add both at the repo root, or ship a root `lmdeck.toml`. |
| `hardware.nodes = N but there are M [[nodes]]` | Make the count match. |
| `[[nodes]] needs exactly one role = "head"` | Exactly one head; all others `worker`. |
| `the head needs a host` | Use `local` or a hosts.toml name; only workers may be `auto`. |
| `command_by_fabric: unknown fabric` | Use one of `single`, `cable`, `mesh`, `ring`, `switch`, `mixed`, `none`. |
| `logs is for detached recipes` | Drop `logs` or set `detached = true`. |
| `no recipe named ...` / ambiguous | Check `lmdeck recipe list` or `doctor`; duplicate names across sources. |
| server never becomes ready | Port mismatch, server not answering `/v1/models`, or `ready_timeout` too short. Check `lmdeck logs <name>`. |
| `port taken` | Another process holds the planned port; set `[engine] port` or stop it. |
| weights missing on a remote host | `lmdeck weights <name>`, pull on the home host, or run with `--on`/auto placement. |
| TensorFold refuses the recipe | It needs pulled weights and a native family; no GGUF/EXL2/AWQ/GPTQ. |
| TabbyAPI refuses the recipe | It needs EXL3/EXL2 weights already on disk; run `lmdeck pull`. |

## Editing this repo

- Rust 2021, MSRV 1.85. Keep dependencies boring and minimal.
- `cargo fmt --all --check`, `cargo clippy --all-targets --locked -- -D
  warnings`, `cargo test --locked` are the CI gates.
- Prefer existing modules and patterns over new abstractions. A new engine
  belongs in `Engine`, `EngineSpec` handling in `src/engine.rs`, the fit rules
  in `src/compat.rs`, README's engine table, and tests.
- Never hand-edit `target/` or cached decks in the repo.
- Tests use `LMDECK_HOME` and temporary directories; keep new tests isolated
  and deterministic. Recipe changes should leave `catalog::builtin()` able to
  parse every built-in.
