```
██╗     ███╗   ███╗██████╗ ███████╗ ██████╗██╗  ██╗
██║     ████╗ ████║██╔══██╗██╔════╝██╔════╝██║ ██╔╝
██║     ██╔████╔██║██║  ██║█████╗  ██║     █████╔╝
██║     ██║╚██╔╝██║██║  ██║██╔══╝  ██║     ██╔═██╗
███████╗██║ ╚═╝ ██║██████╔╝███████╗╚██████╗██║  ██╗
╚══════╝╚═╝     ╚═╝╚═════╝ ╚══════╝ ╚═════╝╚═╝  ╚═╝
```

**lmdeck** is a single-binary CLI for running local LLMs from *recipes*. A recipe
says which model to use, which quant, which engine, and what hardware it
needs. lmdeck shows what's downloaded and what's loaded, starts and stops
servers, searches recipe repos on GitHub and Hugging Face, and checks whether
a recipe fits the machine you're on.

Supported engines: **llama.cpp** (`llama-server`), **vLLM** (native or Docker),
**SGLang** (native or Docker), **TabbyAPI / exllamav3** (EXL3), **LM Studio**
(`lms`), **MLX** (`mlx_lm.server`), plus `custom` for anything with an
OpenAI-compatible `/v1/models`.

It reads [lmswitch](https://github.com/jvr0x/lmswitch) YAML recipes as-is, and
its default source is [`jvr0x/ai-models`](https://github.com/jvr0x/ai-models).

## Install

```sh
cargo install --git https://github.com/jvr0x/lmdeck
```

You need Rust 1.85 or newer. lmdeck runs on Linux and macOS, on both arm64 and
x86_64. Windows isn't supported.

## Quick start

```sh
lmdeck doctor                 # hardware, engines, and which recipes fit
lmdeck source update          # fetch recipes from github:jvr0x/ai-models
lmdeck recipe search qwen --compatible
lmdeck recipe add ai-models/qwen3.6-35b-a3b-nvfp4-fast
lmdeck pull qwen3.6-35b-a3b-nvfp4-fast     # hf download …
lmdeck use 1                  # start deck entry #1, wait for /v1/models
lmdeck list                   # ● loaded  ○ stopped  ✓ downloaded
lmdeck switch qwen3-4b-gguf   # stop the others, start this one
lmdeck stop --all
```

Run `lmdeck` with no arguments to get the wordmark and the deck table. On a
terminal you can then type a row number to toggle that recipe on or off.

## Commands

| command | what it does |
|---|---|
| `lmdeck list [-a] [-l] [-d] [--json]` | Shows the deck with status (●/◐/○), download state (✓/✗), size and port. `-a` lists every known recipe, `-l` only running servers, `-d` the weights on disk (models dir and HF cache). |
| `lmdeck use <ref\|#> [--only] [--no-wait] [--force] [--dry-run]` | Starts a recipe. The aliases `on` and `start` do the same. Before launching it checks compatibility, the engine, free memory and the port. `--dry-run` prints the exact command and runs nothing. |
| `lmdeck switch <ref\|#>` | Same as `use --only`: stops every other lmdeck server first. |
| `lmdeck stop <ref\|#>` / `--all` | Stops a server. lmdeck sends SIGTERM to the process group and SIGKILL after 20 s. Docker recipes get `docker stop`, LM Studio gets `lms unload`. |
| `lmdeck pull <ref> [--dry-run]` | Downloads the weights with `hf download` (or `huggingface-cli`), passing `--revision`, `--include` and `--local-dir` as needed. LM Studio recipes use `lms get` instead. |
| `lmdeck logs <ref> [-n N]` | Prints the tail of the server log. |
| `lmdeck recipe search [words] [--engine E] [--quant Q] [--compatible] [--source S] [--json]` | Searches the deck, the built-ins and every source, with a fit column (✓ ok, ! warning, ✗ won't fit). |
| `lmdeck recipe add <ref\|file\|url> [--name N] [--force]` | Copies a recipe into your deck as TOML. |
| `lmdeck recipe list [--source S]` / `show <ref> [--toml]` / `run <ref>` / `remove <name>` | Lists, inspects, runs or removes recipes. `run` starts a recipe without adding it to the deck. |
| `lmdeck source add <spec> [--name N] [--no-fetch]` / `list` / `update [name]` / `remove <name>` | Manages recipe sources. |
| `lmdeck doctor [--json]` | Shows the platform (GB10, Apple Silicon, NVIDIA, CPU), memory, GPUs and compute capability, which engines are installed, paths, sources, and a per-recipe fit table. |
| `lmdeck init [--models-dir DIR]` | Writes `~/.config/lmdeck/config.toml`. |

A `<ref>` can be a recipe name (`qwen3-4b-gguf`), an `origin/name` pair
(`ai-models/gpt-oss-20b`, `builtin/qwen3-4b-mlx`), or the `#` row number from
`lmdeck list`. When the same name exists in several places, the deck copy wins.

## Sources

```sh
lmdeck source add github:jvr0x/ai-models               # whole repo, default branch
lmdeck source add github:someone/recipes/spark@v2      # subdirectory at a ref
lmdeck source add https://github.com/o/r/tree/main/recipes
lmdeck source add hf:someone/lmdeck-recipes            # a Hugging Face model repo
lmdeck source add hf:datasets/someone/recipes@main     # …or a dataset repo
lmdeck source add ~/src/my-recipes                     # a local directory
```

Sources are fetched over public HTTPS endpoints only:

- **GitHub:** the git trees API (`api.github.com`), then `raw.githubusercontent.com` for the files.
- **Hugging Face:** `huggingface.co/api/.../tree`, then `/resolve/` for the files.

Only `.toml`, `.yaml` and `.yml` files are fetched. A source is capped at 500
files, and each file at 512 KB. Files that aren't recipes, like CI configs or
notes, are skipped without a warning. Recipes that fail to parse are listed by
`lmdeck doctor`.

If `GITHUB_TOKEN`/`GH_TOKEN` or `HF_TOKEN` is set, it's sent as an auth header
to raise rate limits and reach private repos. Tokens are never written to disk.

Each source is mirrored to `~/.cache/lmdeck/sources/<name>/`. Run
`lmdeck source update` to refresh the mirrors.

## Recipe format

lmdeck's own recipes are TOML. YAML with the same schema works too. A recipe's
name defaults to its file name.

```toml
display_name = "Qwen3 30B-A3B (EXL3 4.0bpw)"
description  = "Qwen3 30B-A3B MoE on TabbyAPI / exllamav3."
tags         = ["qwen3", "moe", "dgx-spark"]

[model]
source   = "hf"                            # "hf" (default) or "local"
repo     = "turboderp/Qwen3-30B-A3B-exl3"  # Hugging Face repo id
revision = "4.0bpw"                        # branch / tag / commit
files    = []                              # files or globs (GGUF: first = file to load)
# path   = "turboderp/Qwen3-30B-A3B-exl3-4.0bpw"  # relative to models_dir, or absolute
size_gb  = 16.0                            # used for memory estimates

[quant]
format = "exl3"                            # gguf, exl3, exl2, nvfp4, mxfp4, fp8, awq, gptq, mlx, bf16 …
bits   = 4.0
label  = "4.0bpw"

[engine]
name  = "tabbyapi"     # llama.cpp | vllm | sglang | tabbyapi | lmstudio | mlx | custom
port  = 5000
ctx   = 32768
args  = ["--cache-mode", "Q8"]            # appended last; placeholders below
# env        = { CUDA_VISIBLE_DEVICES = "0" }
# image      = "vllm/vllm-openai:cu130-nightly"   # vllm/sglang: run in Docker
# entrypoint = ["vllm", "serve"]                  # docker --entrypoint override
# volumes    = ["/data:/data:ro"]
# bin        = "/opt/llama.cpp/build/bin/llama-server"
# dir        = "~/tabbyAPI"                       # tabbyapi checkout
# command    = ["my-server", "--port", "{port}"]  # custom engines
# ready_timeout = 900

[hardware]             # all optional
min_memory_gb = 24     # unified memory (GB10, Apple), VRAM (discrete GPUs) or RAM (CPU)
accelerator   = ["nvidia"]   # nvidia | apple | amd | cpu
os            = ["linux"]    # linux | macos
arch          = []           # aarch64 | x86_64
min_compute_capability = "10.0"   # e.g. NVFP4 needs Blackwell (GB10 = 12.1)
nodes         = 1
```

Placeholders in `args` and `command`: `{name}`, `{port}`, `{ctx}`, `{model}`,
`{models_dir}`, `{host}`, `{repo}`, `{revision}`.

If `args` already sets one of lmdeck's default flags (for example
`--n-gpu-layers` or `--gpu-memory-utilization`), lmdeck doesn't add its
default.

### What each engine runs

| engine | command (simplified) |
|---|---|
| `llama.cpp` | `llama-server --model <models_dir>/<repo>/<file> --alias <name> --host --port --ctx-size …`. If the GGUF isn't on disk, it uses `--hf-repo <repo> --hf-file <file>` instead. |
| `vllm` | Native: `vllm serve <repo or path> --served-model-name <name> --host --port --max-model-len [--revision]`. With `image`, it runs `docker run --rm --name lmdeck-<name> --gpus all --network host --ipc host` with the HF cache mounted. `HF_TOKEN` is forwarded by name only. |
| `sglang` | `python3 -m sglang.launch_server --model-path … --served-model-name … --context-length …`, natively or in Docker. |
| `tabbyapi` | `python3 main.py --model-dir … --model-name … --backend exllamav3 --max-seq-len …`, run in the TabbyAPI checkout. Weights must be local: `lmdeck pull` puts EXL3 branches in `<models_dir>/<repo>-<revision>`. |
| `lmstudio` | `lms server start`, then `lms load <model> --identifier <name>`. `stop` runs `lms unload`. |
| `mlx` | `mlx_lm.server --model <repo or path> --host --port`. |
| `custom` | Your `command`. |

### lmswitch compatibility

lmswitch YAML recipes are converted when loaded. The mapping:

- `runtime` becomes the engine:
  - `llama` → llama.cpp
  - `vllm` → vllm
  - `sglang` → sglang
  - `exllamav3` → tabbyapi
  - `*-dual` → `nodes = 2`
- `model: org/repo/file.gguf` becomes `repo` plus `files`. It also becomes `path`, resolved against `models_dir`, the same `~/models` layout lmswitch uses.
- These keys carry over into engine flags:
  - `gpu_layers`, `threads`, `batch`, `ubatch`, `mmproj`
  - llama.cpp's `fit` (defaults to `-fit off`, as in lmswitch)
  - `gpu_memory_utilization`, `max_num_seqs`, `enforce_eager`
  - `tool_call_parser`, `reasoning_parser`, `trust_remote_code`
  - `image`, `entrypoint`, `env`, `extra_mounts`, `extra_args`
- The recipe description comes from the file's first comment line.

Multi-node (`*-dual`) recipes are listed and checked, but v0.1 refuses to
launch them. Use lmswitch for those.

## Files

| what | where |
|---|---|
| config | `~/.config/lmdeck/config.toml` (`$XDG_CONFIG_HOME`) |
| your deck | `~/.config/lmdeck/recipes/*.toml` |
| source mirrors | `~/.cache/lmdeck/sources/` (`$XDG_CACHE_HOME`) |
| run records and logs | `~/.local/state/lmdeck/{run,logs}/` (`$XDG_STATE_HOME`) |
| model weights | `models_dir` (default `~/models`, or `$LMDECK_MODELS_DIR`) plus the Hugging Face cache |

Set `LMDECK_HOME=/some/dir` to put all of lmdeck's files under one directory.
The tests use this. macOS uses the same XDG layout.

```toml
# ~/.config/lmdeck/config.toml
models_dir = "~/models"
host = "0.0.0.0"

[engines]                 # only needed when auto-detection misses something
# llama_server = "~/llama.cpp/build/bin/llama-server"
# tabbyapi_dir = "~/tabbyAPI"
# python = "~/venvs/vllm/bin/python"
# lms = "~/.lmstudio/bin/lms"
# docker = "docker"

[[sources]]
name = "ai-models"
spec = "github:jvr0x/ai-models"
```

## Built-in examples

These recipes ship inside the binary. `lmdeck list` shows them while your deck
is empty. Every Hugging Face id below was checked against the Hub.

| recipe | model | engine | for |
|---|---|---|---|
| `qwen3-4b-gguf` | `unsloth/Qwen3-4B-GGUF` · Q4_K_M | llama.cpp | anything, ~2.5 GB |
| `gpt-oss-20b-gguf` | `unsloth/gpt-oss-20b-GGUF` · Q4_K_M | llama.cpp | 16 GB+ |
| `qwen3-30b-a3b-exl3` | `turboderp/Qwen3-30B-A3B-exl3` @ 4.0bpw | tabbyapi | NVIDIA / DGX Spark |
| `qwen3.6-35b-a3b-nvfp4-spark` | `nvidia/Qwen3.6-35B-A3B-NVFP4` | vllm (Docker) | DGX Spark / Blackwell |
| `qwen3-4b-mlx` | `mlx-community/Qwen3-4B-4bit` | mlx | Apple Silicon |
| `qwen3-4b-lmstudio` | `unsloth/Qwen3-4B-GGUF` | lmstudio | LM Studio |

## Development

```sh
cargo build --release
cargo test                      # unit and integration tests, no network needed
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## License

MIT
