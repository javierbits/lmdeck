# Multi-host lmdeck: remote control, weights, run-on, multi-node, topology

lmdeck runs one `lmdeck serve` on every machine (DGX Sparks, a Mac). One
machine, the **controller**, lists the others in `~/.config/lmdeck/hosts.toml`
and drives them through their `serve` API. The controller is wherever you run
`lmdeck` / `lmdeck tui` / the dashboard. It has no special install.

> **Status: UNTESTED on real hardware.** Everything here was built and tested
> on a CPU-only Linux VM. Remote control, weights indexing, the run-on planner
> and multi-node ordering were exercised end to end against real
> `lmdeck serve` processes on that one VM (the "rig" below), with stub
> servers instead of inference engines. CX7 detection is tested against
> fixtures written to look like a Spark's `ip -j addr`, `/sys/class/net` and
> RDMA layout; they were **not** captured from a real Spark. NFS exports,
> docker NFS volumes, Ray and vLLM were never run.

What this mirrors (read, not modified, not copied):

- **Mia's `WORKER_WEIGHTS=nfs`** (MiaAI-Lab Ornith TensorFold scripts): the
  weights live once on their home Spark; the other Spark mounts the Hugging
  Face cache read-only over the CX7 link as a docker NFS volume at
  `/root/.cache/huggingface`, after a `test -f` / `find -L` preflight.
- **lmswitch's `vllm-dual`** (`jvr0x/lmswitch`, read only): CX7 IPs as
  `master_addr` / `worker_ip`, NCCL pinned to the CX7 interface
  (`NCCL_SOCKET_IFNAME`), RDMA device (`NCCL_IB_HCA`) and RoCE GID index
  (`NCCL_IB_GID_INDEX`), and per-node compile caches kept **off** NFS.

## 1. Remote control

```sh
# on each machine (LAN / Tailscale address, a token is generated if you don't give one)
lmdeck serve --host 100.101.102.103 --token "$(openssl rand -hex 16)"
# on the controller
lmdeck host add spark2 http://spark2:7979 --token <that token>
```

| where | how |
|---|---|
| CLI | `lmdeck start <recipe> --on spark2`, `lmdeck switch <recipe> --on spark2`, `lmdeck stop <recipe> --on spark2`, `lmdeck logs <recipe> --on spark2 [-n 200]` |
| TUI, Hosts tab (`2`) | `↑↓` host, `←→` model on that host, `↵`/space start the Deck tab's selected recipe there, `s` switch to it there, `x` stop the selected model, `l` its logs (fetched through that host's serve, polled every 2 s) |
| web | host cards: `≡` logs and `■` stop per model, **run here** picker; recipe cards: **Run on another host…** |

Every call goes to that host's own `lmdeck serve`
(`/api/hosts/<host>/{start,stop,switch}/<recipe>` on the controller proxies
it). A host only runs recipes it already has by name, unless its config has
`[serve] accept_recipes = true`, which lets a controller upload a recipe
(`POST /api/run`). A recipe is a command line, so that is off by default.

### Tokens

Per host in `hosts.toml`, first match wins:

1. `token = "…"` (the file is written `0600`),
2. `token_env = "MY_VAR"`, the name of an environment variable,
3. `LMDECK_HOST_TOKEN_<NAME>` (name upper-cased, `-` and `.` become `_`).

On the serving side: `--token`, `LMDECK_TOKEN`, or `[serve] token_env`.
Tokens are never written to the repo or logs.

### Auth rules (`lmdeck serve`)

- **Bind defaults stay safe.** `127.0.0.1` by default. Binding anything
  else without a token generates one and prints it.
- **Reads.** On loopback, reads need no token (the Host header must be
  localhost, against DNS rebinding). On any other bind, every `/api` route
  except `/api/health` needs the token, **reads included**: telemetry, logs
  and weights paths say a lot about a machine. Send
  `Authorization: Bearer <token>` (or `?token=` for EventSource).
- **Mutations** need `X-Lmdeck: 1` (a cross-site form can't send it), and
  **a state-changing request from a non-loopback peer without a configured
  token is refused (403)** whatever the bind. This last rule is defense in
  depth; its non-loopback path is not covered by an integration test (the
  test machine has no second interface).
- No TLS of its own: use Tailscale, a VPN, or an HTTPS reverse proxy.
- **Harness sync spans hosts.** Each machine writes the harness configs on
  *itself* for the models it serves (opencode, omp, grok, pi, hermes — see
  the README's Harness sync), and every lmdeck also asks its configured
  hosts what *they* are serving: sparkA's opencode lists sparkB's running
  model (base URL pointed at sparkB), no copy involved. `lmdeck harness
  hosts all|none|NAME…` and `[harness] hosts` choose which hosts count. A
  `[[nodes]]` recipe syncs as one lane named after the recipe, pointed at
  its head (not `<recipe>.head`); workers get none. A recipe with
  `[harness] chat = false` (not a chat model) is left out wherever it runs:
  each host's snapshot says so.


## 2. Where the weights live


Every `serve` indexes its own weights (Hugging Face cache snapshots and files
under `models_dir`) and reports them at `GET /api/weights`, a cache repo's
GGUF files by name (`ggufs`), so a recipe is found only where its own quant
is. The controller merges them (cached 60 s):

```sh
lmdeck weights [--refresh] [--json]
```

They show up as **weights on: spark2** on web cards, `⛁spark2` in the TUI
list (the `dl` column shows `⛁` when the weights are only on another host)
and `on ⛁ spark2 · home spark2` in the TUI details. The **home** is the host
that serves them to others.

## 3. Choosing where a model runs (`--on`)

```sh
lmdeck start qwen3.6-35b-a3b-nvfp4-spark --on spark1 --dry-run   # show the plan
lmdeck start qwen3.6-35b-a3b-nvfp4-spark --on spark1
```

Also the TUI picker (`o` on the Deck tab) and the web **Run on another host…**
dropdown. Both show, per host: weights here / `⇄ nfs ro` / no weights /
can't, the link, a load-time estimate (`size / link speed`, e.g. "≈12 s to
stream 23.5G at 200 Gb/s"), and the fit check against **the run host's**
memory.

**Weights are never copied.** No rsync, no copy, no tmpfs: there is no such
mode. If the weights are on another host, the plan is Mia's
`WORKER_WEIGHTS=nfs`:

1. On the home: `echo '<hf home> <run host CX7 IP>(ro,no_subtree_check)' | sudo tee -a /etc/exports`
   and `sudo exportfs -ra`. lmdeck **only runs these** when the home's
   config has `[weights] allow_export = true` (and then with `sudo -n`, so a
   password prompt fails instead of hanging). Otherwise it prints them and
   stops. It never edits `/etc/exports` silently, and skips the line if an
   equivalent export is already there.
2. On the run host: `docker volume create --driver local --opt type=nfs --opt device=:<hf home> --opt o=addr=<home CX7 IP>,nfsvers=4.2,ro,… lmdeck-hf-<home>`.
3. Preflight inside the image, through the volume:
   `test -f …/snapshots/<rev>/config.json` and `find -L …/snapshots/<rev> -type f`.
4. The container starts with the volume at `/root/.cache/huggingface:ro`
   and `HF_HUB_OFFLINE=1`.

This works for Docker recipes (vLLM, SGLang, LMDeploy in Docker). For a
native engine, or weights under `models_dir` rather than the HF cache, the
plan refuses and says to run on the home or mount by hand.

**No ConnectX-7 link, no streaming.** Without a CX7 link between the two
hosts the plan is refused. `--allow-slow-link` (or the web/TUI equivalent)
allows LAN / Tailscale and adds a loud warning.

## 4. Multi-node recipes (`[[nodes]]`)

One recipe, several machines. Each `[[nodes]]` entry is a command for one
host (`host = "local"`, a hosts.toml name, or `"auto"` for a worker lmdeck
places, see below), with exactly one `role = "head"`. lmdeck:

- starts them **in order** (`[cluster] order = "head-first"` by default),
  waiting for each node's **readiness gate** before the next: `ready_port`
  answers on TCP if set; the last node, when it is the head, serves
  `/v1/models`; otherwise the node is still running after `ready_delay`
  seconds (default 3);
- stops them in **reverse** order, and if a node fails to come up, stops the
  ones already started and shows that node's log;
- reports one aggregate status and per-node status; `lmdeck logs <recipe> --node w1` reads one node's log;
- mounts the weights (`[model]`) only on the nodes whose command or env
  uses `{weights_mount}` or `{weights_volume}`: the weights home bind-mounts
  its own cache, every other such node gets the NFS export, docker volume
  and preflight below. A node that takes no mount (llama.cpp's
  `rpc-server`, which gets its layers from the head) gets none of it, and
  `--dry-run` says `weights: not mounted`;
- checks each node's memory against **its share** of the weights: the size
  (on the weights home's disk, else `size_gb`) divided by the number of
  nodes, or `[hardware] min_memory_gb` when the recipe sets it, which counts
  per node. `--dry-run` prints it as `memory: needs ~N GB` under each node;
- with a `[source]`, runs every node from its own checkout
  (`checkouts/<recipe>.<node>/` on its machine) at **one commit**: a start
  brings the controller's `checkouts/<recipe>/` to the recipe's `ref`
  (fetching when the checkout isn't there yet) and pins each node to that
  commit, and a node whose checkout is elsewhere fetches and checks it out.
  A changed `ref`, or `lmdeck pull <recipe>` moving a branch, reaches every
  node at its next start;

- fills fast-link facts into each node's command: `{head_addr}`,
  `{node_addr}`, `{ifname}`, `{hca}`, `{gid_index}`, `{weights_mount}`,
  `{image}`, `{head_port}`, `{node}`, `{rank}`, `{nodes}`, and the fabric:
  `{fabric}` (`ring`, `switch`, `mesh`, `cable`, ...), `{fabric_order}` (the
  hosts in fabric order, comma-separated) and, on a switchless ring,
  `{ring_rank}`, `{ring_prev}`, `{ring_next}`, `{ring_prev_addr}`,
  `{ring_next_addr}`, `{ring_prev_ifname}`, `{ring_next_ifname}` (each
  neighbour's CX7 address on the shared cable and this node's port to it);
- tells each node about the others, in `[[nodes]]` order with itself left
  out: `{peer_addr}` / `{peer_host}` for the first other node,
  `{peer2_addr}` / `{peer2_host}` for the second, and so on (no `peer1`).
  `{peer…_addr}` is that node's CX7 address on the link from this node
  (through the hosts cabled between them when they share no cable, with a
  warning; `127.0.0.1` when both run on one machine), `{peer…_host}` its
  hosts.toml name (this machine's `[node] name` for `local`). A llama.cpp
  RPC head writes `--rpc {peer_addr}:50052` instead of a worker address. A
  peer past the node count fails the plan;
- fills `{user}` (login name) and `{home}` (home directory) on the machine
  each node runs on, with the engine placeholders (`{model}`, `{port}`, …)
  and what that machine has configured: `{hf_home}`, the `[engines]`
  binaries (`{llama_server}`, `{python}`, `{docker}`, …) and their
  directories (`{llama_server_dir}/rpc-server`), `{tabbyapi_dir}`, and
  `{model_dir}`: the directory holding `[model]`'s first file on that
  machine, its Hugging Face cache snapshot first (README, "Recipe format").
  With `files = ["UD-IQ4_NL/X-00001-of-00005.gguf"]` a llama.cpp head
  writes `--model {model_dir}/X-00001-of-00005.gguf` and reaches the repo's
  `mmproj-F16.gguf` as `{model_dir}/../mmproj-F16.gguf`. A node whose
  machine lacks one fails to start, saying which `[engines]` key to set
  there or that the weights aren't on it; a placeholder nothing fills fails
  the plan on the controller, `--dry-run` included;
- is **engine-agnostic**: the commands decide what runs (Ray + vLLM, SGLang,
  anything).

Both UIs show it as one recipe: `×2` / `×2 nodes`, a node list with each
node's host and status, and the cluster error if any. Run-on doesn't apply
(the recipe says where each node runs).

### `host = "auto"`

A worker can leave its machine to lmdeck: `host = "auto"`. When the start is
planned (`--dry-run` too, which shows `on gigabyte (auto)`), lmdeck reads the
CX7 fabric across this machine and every hosts.toml host and picks from the
machines that answer and have a CX7 path to the head, skipping any host
another node names:

- switchless **ring**: the head's cable neighbours, next then previous in
  the cabled order, then onward around the ring from next. The picks are
  ranked in cabled order from the head, so with every Spark in use the ranks
  walk the cables (no "skip cables" warning).
- **cable**: the other Spark.
- **switch**, **mesh** or anything else: the machines sharing a CX7 segment
  with the head, then the ones reached through other machines' cables, each
  group in rack order (this machine, then hosts.toml).

The choice is deterministic. With too few machines the plan fails and names
the free ones, the ones other nodes hold, and the ones offline or without a
CX7 path to the head. The head itself needs a host (`"local"` or a
hosts.toml name). A running recipe's nodes stay where its start put them
(`lmdeck logs <recipe> --node w1`, stop and the status views read it).

`examples/vllm-ray-2spark.toml` is a 2-Spark vLLM + Ray example.
**UNTESTED**: nobody has run it on real Sparks. It follows lmswitch's
`vllm_dual_ray` sequence and NCCL wiring and Mia's NFS weights, and keeps the
vLLM / torch compile caches on each node's local disk.

## 5. Fabric auto-detect and topology

Each serve answers `GET /api/fabric` with its NICs, GPUs, DMI/`hw.model`
product string, and LLDP neighbours — read-only, no sudo: `ip -j addr` /
`ip -j link` (fallback `/sys/class/net`), speed, operstate, MTU, driver
(`mlx5_core`), RDMA devices from `/sys/class/net/<if>/device/infiniband`,
`rdma -j link` if present, the RoCE v2 IPv4 GID index, `nvidia-smi` (every
GPU), and `lldpctl` when installed. Missing tools are listed, not errors.

The controller builds an **L2 segment graph** (not a host↔host mesh):

- Hosts that share a subnet form a *segment* (multiple ports of the same
  host on that subnet collapse into one member with rails).
- **Two hosts** on a point-to-point segment (`/30`/`/31`, or exactly two
  members with no LLDP bridge) → a direct **cable**.
- **Three or more** members, or an LLDP neighbour advertising Bridge /
  a switch-like chassis name (the cluster's own hosts don't count: Sparks
  running lldpd announce Bridge) → a synthetic **switch** node with star
  edges to each host. No phantom pairwise CX7 edges through the switch.
- Any number of hosts (1…32+), switches, rails, and mixed CX7 / LAN /
  Tailscale. Single-host view is a centred node with a hint to add peers;
  two hosts sit side by side. Layout is seeded and overlap-free.
- Placement / multi-node `head_addr` / `ifname` / `hca` use
  `path_between` on the graph (shared segment preferred), not pair lookups.
- The **fabric shape** across the hosts with CX7 ports: `single`, `cable`
  (two Sparks), `mesh` (every pair cabled, such as a triangle), `ring`,
  `switch` (all on one CX7 switch subnet, any count), `mixed` or `none`. A
  `ring` is four or more Sparks without a switch, each with exactly two
  cable neighbours, forming one cycle. Its order is the **cabled order**,
  walked from the first host: a folded ring racked 1-2-3-4 and cabled
  1-2-4-3-1 reads `spark1 → spark2 → spark4 → spark3 → spark1`. Multi-node
  ranks, the TUI and web diagrams, and `lmdeck hosts topology` all follow it.
  A `[[nodes]]` recipe whose ranks skip cables gets a warning naming the
  cabled order to list them in. Two hosts across the ring get the CX7 path
  through a neighbour, with a warning that the neighbour routes it (or the
  recipe relays it, as Mia's `start-switchless.sh` does).

Links are verified with a ping / TCP check unless `--no-verify`.

Overrides in `hosts.toml` win over detection:

```toml
[[host]]
name = "spark2"
url = "http://spark2:7979"
token_env = "SPARK2_TOKEN"
link_addr = "10.10.12.2"     # this host's CX7 address for every peer

[[link]]                     # one pair, when detection can't see it
a = "local"
b = "spark2"
a_addr = "10.10.12.1"
b_addr = "10.10.12.2"
a_ifname = "enp1s0f0np0"
a_hca = "rocep1s0f0"
a_gid_index = 3
gbps = 200
kind = "cx7"                 # or lan / tailscale
```

Views:

- `lmdeck hosts topology [--json] [--no-verify]`
- TUI tab `3` **Topology**: host boxes with braille device icons, switch
  hubs, cables (`● busy` when NFS weights or a cluster use them), a link
  table; falls back to a segment list when the pane is tight.
- Web dashboard **Topology** section: SVG with theme-tinted dot-art icons
  (Spark / Mac / PC / switch / GPU), switches as hubs, bundled rails,
  management dotted, busy pulse (honours `prefers-reduced-motion`).

`LMDECK_DEMO=1` draws simulated fabric from built-in fixtures, labelled
**DEMO DATA**. Shape via `LMDECK_DEMO_TOPO`: `tri` (default), `duo`,
`switch8`, `single`, `mix`, `ring4` (four Sparks cabled 1-2-3-4-1) and
`fold4` (the folded ring, cabled 1-2-4-3-1).

## Screenshots

Taken on the single-VM rig (three `lmdeck serve` instances, `spark1`
controlling `spark2` and `spark3`; their `[[link]]` entries were written by
hand over loopback addresses, so the "cx7 200G" labels there are **not**
real CX7 cables) unless marked DEMO DATA.

| | |
|---|---|
| ![](screenshots/tui-hosts-controls.png) | TUI Hosts tab on spark2: its two running stub servers, `x stop · l logs`, `↵ run <deck pick> here`. |
| ![](screenshots/tui-run-on.png) | TUI run-on picker (`o`): Qwen3 0.6B's weights are on spark2, so spark1 and spark3 would mount them read-only over NFS. |
| ![](screenshots/tui-multinode.png) | A 2-node recipe (python stubs) running: head on this machine, worker on spark2, started in order. |
| ![](screenshots/tui-weights-on.png) | Weights on other hosts: `⛁spark2` / `⛁spark3` in the list, `on ⛁ spark2 · home spark2` in details. Real downloaded files. |
| ![](screenshots/tui-topology-1host.png) | TUI Topology, **DEMO DATA** (`LMDECK_DEMO_TOPO=single`): one centred host. |
| ![](screenshots/tui-topology-2host.png) | TUI Topology, **DEMO DATA** (`duo`): two Sparks on a CX7 cable. |
| ![](screenshots/tui-topology-demo.png) | TUI Topology, **DEMO DATA** (`tri`): triangle of /30 cables. |
| ![](screenshots/tui-topology-switch8.png) | TUI Topology, **DEMO DATA** (`switch8`): eight Sparks on `SN2100-TOR`. |
| ![](screenshots/web-run-on.png) | Web run-on dialog: the full NFS plan for running on spark1. Nothing runs until **Start there**. |
| ![](screenshots/web-hosts-controls.png) | Web host cards with per-model logs / stop and **run here**. |
| ![](screenshots/web-weights-on.png) | Web card with **weights on: spark2** and the run-on dropdown. |
| ![](screenshots/web-multinode-card.png) | Web card for the 2-node stub: `×2 nodes`, start order and per-node status. |
| ![](screenshots/web-topology-rig.png) | Web topology of the rig (hand-written loopback links, busy link to spark2 carries the 2-node stub). |
| ![](screenshots/web-topology-1host.png) | Web topology, **DEMO DATA** (`single`): one host + hint to add peers. |
| ![](screenshots/web-topology-2host.png) | Web topology, **DEMO DATA** (`duo`): two Sparks on CX7 + offline Mac (icon). |
| ![](screenshots/web-topology-demo.png) | Web topology, **DEMO DATA** (`tri`): triangle + LAN switch + offline Mac. |
| ![](screenshots/web-topology-switch8.png) | Web topology, **DEMO DATA** (`switch8`): eight Sparks on one CX7 switch. |
| ![](screenshots/web-topology-gpu-closeup.png) | Node close-up: **2× RTX 4090** cards (demo), temps / util / power. |
| ![](screenshots/web-topology-mac-closeup.png) | Mac node close-up: 12×8 dot-matrix icon, offline (demo) tint. |
