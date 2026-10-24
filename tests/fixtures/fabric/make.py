#!/usr/bin/env python3
"""Writes the fabric fixtures used by `src/fabric.rs` tests and `LMDECK_DEMO=1`.

These are *constructed* in the format DGX Spark's `ip -j addr`, `rdma -j link`,
`ibv_devices`, `nvidia-smi`, `lldpctl` and sysfs produce; they were not captured
from hardware. Re-run after editing: `python3 tests/fixtures/fabric/make.py`.
"""
import json, os

HERE = os.path.dirname(os.path.abspath(__file__))
CX7 = [("enp1s0f0np0", "rocep1s0f0"), ("enp1s0f1np1", "rocep1s0f1"),
       ("enP2p1s0f0np0", "roceP2p1s0f0"), ("enP2p1s0f1np1", "roceP2p1s0f1")]
NVSMI = "nvidia-smi --query-gpu=name,memory.total,compute_cap,driver_version --format=csv,noheader,nounits"
NVSMI_TEL = "nvidia-smi --query-gpu=index,name,temperature.gpu,power.draw,power.limit,utilization.gpu,memory.used,memory.total,clocks.sm --format=csv,noheader,nounits"


def gid(ip):
    a, b, c, d = (int(x) for x in ip.split("."))
    return f"0000:0000:0000:0000:0000:ffff:{a:02x}{b:02x}:{c:02x}{d:02x}"


def lldp_switch(ifname, chassis="SN2100-TOR", port="Eth1/1"):
    """Minimal lldpctl -f json with a Bridge-capable neighbour (a switch)."""
    return json.dumps({
        "lldp": {
            "interface": [{
                ifname: {
                    "via": "LLDP",
                    "chassis": {
                        chassis: {
                            "id": {"type": "mac", "value": "7c:fe:90:11:22:33"},
                            "name": {"value": chassis},
                            "descr": {"value": "NVIDIA Spectrum"},
                            "capability": [
                                {"type": "Bridge", "enabled": True},
                                {"type": "Router", "enabled": True},
                            ],
                        }
                    },
                    "port": {
                        "id": {"type": "ifname", "value": port},
                        "descr": {"value": port},
                    },
                }
            }]
        }
    })


def spark(host, n, addrs, lan, ts, up=(), rdma_tool=True, ibv=True, lldp=None,
          hostname=None, gpus=None, dmi_product=None):
    """addrs: {ifname: "ip/prefix"} on CX7 ports; up: CX7 ports that are up without an address.
    lldp: optional {ifname: (chassis, port)} for switch neighbours.
    gpus: nvidia-smi inventory line(s); default one GB10.
    """
    files, dirs, links, cmds = {}, {}, {}, {}
    ip_addr, ip_link = [], []
    idx = 1

    def iface(name, mac, state, mtu, v4=None, driver=None, speed=None, link_type="ether"):
        nonlocal idx
        e = {"ifindex": idx, "ifname": name, "flags": ["BROADCAST", "MULTICAST"] + (["UP", "LOWER_UP"] if state == "UP" else []),
             "mtu": mtu, "qdisc": "mq", "operstate": state, "group": "default", "txqlen": 1000,
             "link_type": link_type, "address": mac, "broadcast": "ff:ff:ff:ff:ff:ff"}
        idx += 1
        ip_link.append(dict(e))
        ai = []
        if v4:
            ip, pl = v4.split("/")
            ai.append({"family": "inet", "local": ip, "prefixlen": int(pl), "scope": "global", "label": name,
                       "valid_life_time": 4294967295, "preferred_life_time": 4294967295})
        e["addr_info"] = ai
        ip_addr.append(e)
        sys = f"/sys/class/net/{name}"
        files[f"{sys}/operstate"] = state.lower()
        files[f"{sys}/mtu"] = str(mtu)
        files[f"{sys}/address"] = mac
        if speed is not None:
            files[f"{sys}/speed"] = str(speed)
        if driver:
            links[f"{sys}/device/driver"] = f"../../../../bus/pci/drivers/{driver}"
    iface("lo", "00:00:00:00:00:00", "UNKNOWN", 65536, "127.0.0.1/8", link_type="loopback")
    iface("enP7s7", f"4c:bb:47:0{n % 10}:00:01", "UP", 1500, lan, driver="r8169", speed=10000)
    rdma_links = []
    for i, (nd, dev) in enumerate(CX7):
        v4 = addrs.get(nd)
        state = "UP" if (v4 or nd in up) else "DOWN"
        iface(nd, f"4c:bb:47:1{n % 10}:0{i}:00", state, 9000 if v4 else 1500, v4, driver="mlx5_core",
              speed=200000 if state == "UP" else -1)
        dirs[f"/sys/class/net/{nd}/device/infiniband"] = [dev]
        dirs[f"/sys/class/infiniband/{dev}/device/net"] = [nd]
        g = f"/sys/class/infiniband/{dev}/ports/1"
        dirs[f"{g}/gids"] = ["0", "1", "2", "3"]
        files[f"{g}/gids/0"] = f"fe80:0000:0000:0000:4ebb:47ff:fe1{n % 10}:0{i}00"
        files[f"{g}/gid_attrs/types/0"] = "IB/RoCE v1"
        files[f"{g}/gids/1"] = f"fe80:0000:0000:0000:4ebb:47ff:fe1{n % 10}:0{i}00"
        files[f"{g}/gid_attrs/types/1"] = "RoCE v2"
        if v4:
            ip = v4.split("/")[0]
            files[f"{g}/gids/2"] = gid(ip)
            files[f"{g}/gid_attrs/types/2"] = "IB/RoCE v1"
            files[f"{g}/gids/3"] = gid(ip)
            files[f"{g}/gid_attrs/types/3"] = "RoCE v2"
        rdma_links.append({"ifindex": i, "ifname": f"{dev}/1", "port": 1, "state": "ACTIVE" if state == "UP" else "DOWN",
                           "physical_state": "LINK_UP" if state == "UP" else "DISABLED", "netdev": nd, "netdev_index": i + 3})
    dirs["/sys/class/infiniband"] = [d for _, d in CX7]
    iface("tailscale0", "", "UNKNOWN", 1280, ts, link_type="none")
    iface("docker0", f"02:42:ac:11:00:{n:02x}", "DOWN", 1500, "172.17.0.1/16", driver=None)
    dirs["/sys/class/net"] = [e["ifname"] for e in ip_link]
    cmds["ip -j addr"] = json.dumps(ip_addr)
    cmds["ip -j link"] = json.dumps(ip_link)
    if rdma_tool:
        cmds["rdma -j link"] = json.dumps(rdma_links)
    if ibv:
        cmds["ibv_devices"] = "    device                 node GUID\n    ------              ----------------\n" + "".join(
            f"    {d:<20}4cbb4703001{n % 10}000{i}\n" for i, (_, d) in enumerate(CX7))
    if lldp:
        # merge all ifaces into one lldpctl document
        ifaces = []
        for ifn, (chassis, port) in lldp.items():
            doc = json.loads(lldp_switch(ifn, chassis, port))
            ifaces.extend(doc["lldp"]["interface"])
        cmds["lldpctl -f json"] = json.dumps({"lldp": {"interface": ifaces}})
    cmds[NVSMI] = gpus or "NVIDIA GB10, [N/A], 12.1, 580.95.05\n"
    if dmi_product:
        files["/sys/class/dmi/id/product_name"] = dmi_product
        files["/sys/class/dmi/id/sys_vendor"] = "NVIDIA"
    return {"hostname": hostname or host, "commands": cmds, "files": files, "dirs": dirs, "links": links}


def plain(host, with_ip, gpus=None):
    files = {"/sys/class/net/lo/operstate": "unknown", "/sys/class/net/eno1/operstate": "up",
             "/sys/class/net/eno1/speed": "1000", "/sys/class/net/eno1/mtu": "1500",
             "/sys/class/net/eno1/address": "a8:a1:59:00:00:01"}
    cmds = {}
    if with_ip:
        cmds["ip -j addr"] = json.dumps([
            {"ifindex": 1, "ifname": "lo", "mtu": 65536, "operstate": "UNKNOWN", "link_type": "loopback",
             "address": "00:00:00:00:00:00", "addr_info": [{"family": "inet", "local": "127.0.0.1", "prefixlen": 8}]},
            {"ifindex": 2, "ifname": "eno1", "mtu": 1500, "operstate": "UP", "link_type": "ether",
             "address": "a8:a1:59:00:00:01", "addr_info": [{"family": "inet", "local": "192.168.1.50", "prefixlen": 24},
                                                           {"family": "inet6", "local": "fe80::aaa1:59ff:fe00:1", "prefixlen": 64}]}])
    if gpus:
        cmds[NVSMI] = gpus
    return {"hostname": host, "commands": cmds, "files": files,
            "dirs": {"/sys/class/net": ["lo", "eno1"]},
            "links": {"/sys/class/net/eno1/device/driver": "../../../../bus/pci/drivers/e1000e"}}


def workstation(host, lan, gpus):
    """A regular PC with several discrete RTX cards on LAN only (no CX7)."""
    body = plain(host, True, gpus=gpus)
    # Rewrite LAN address
    addrs = json.loads(body["commands"]["ip -j addr"])
    for a in addrs:
        if a["ifname"] == "eno1":
            a["addr_info"][0]["local"] = lan.split("/")[0]
    body["commands"]["ip -j addr"] = json.dumps(addrs)
    body["hostname"] = host
    body["files"]["/sys/class/dmi/id/product_name"] = "Precision 7960"
    body["files"]["/sys/class/dmi/id/sys_vendor"] = "Dell Inc."
    return body


out = {
    # Two Sparks, one QSFP cable on port 1 (both twins addressed on the subnet: two rails).
    "duo-spark1": spark("spark1", 1, {"enp1s0f1np1": "192.168.100.10/24", "enP2p1s0f1np1": "192.168.100.12/24"},
                        "192.168.1.21/24", "100.101.1.1/32",
                        hostname="spark-01.lab", dmi_product="DGX Spark"),
    "duo-spark2": spark("spark2", 2, {"enp1s0f1np1": "192.168.100.11/24", "enP2p1s0f1np1": "192.168.100.13/24"},
                        "192.168.1.22/24", "100.101.1.2/32",
                        hostname="spark-02.lab", dmi_product="DGX Spark"),
    # Three Sparks in a triangle: one /30 per cable, as in Mia's start-tp3.sh; no rdma/ibv tools installed.
    "tri-spark1": spark("spark1", 1, {"enp1s0f0np0": "10.10.12.1/30", "enp1s0f1np1": "10.10.13.1/30"},
                        "192.168.1.21/24", "100.101.1.1/32", up=("enP2p1s0f0np0", "enP2p1s0f1np1"),
                        rdma_tool=False, ibv=False, hostname="spark-a.lab", dmi_product="DGX Spark"),
    "tri-spark2": spark("spark2", 2, {"enp1s0f0np0": "10.10.12.2/30", "enp1s0f1np1": "10.10.23.1/30"},
                        "192.168.1.22/24", "100.101.1.2/32", up=("enP2p1s0f0np0", "enP2p1s0f1np1"),
                        rdma_tool=False, ibv=False, hostname="spark-b.lab", dmi_product="DGX Spark"),
    "tri-spark3": spark("spark3", 3, {"enp1s0f0np0": "10.10.13.2/30", "enp1s0f1np1": "10.10.23.2/30"},
                        "192.168.1.23/24", "100.101.1.3/32", up=("enP2p1s0f0np0", "enP2p1s0f1np1"),
                        rdma_tool=False, ibv=False, hostname="spark-c.lab", dmi_product="DGX Spark"),
    # A box with one GbE port and no RDMA: without and with iproute2.
    "nordma": plain("box", False),
    "nordma-ip": plain("box", True),
    # Workstation with two discrete RTX cards (no CX7).
    "pc-2x-rtx": workstation("workstation", "192.168.1.60/24",
                             "NVIDIA GeForce RTX 4090, 24564, 8.9, 560.35.03\n"
                             "NVIDIA GeForce RTX 4090, 24564, 8.9, 560.35.03\n"),
}

# Eight Sparks on one CX7 switch subnet (/24), with LLDP naming the TOR.
for i in range(1, 9):
    ip = f"10.20.0.{10 + i}/24"
    out[f"sw8-spark{i}"] = spark(
        f"spark{i}", i,
        {"enp1s0f0np0": ip},
        f"192.168.1.{20 + i}/24", f"100.101.1.{i}/32",
        lldp={"enp1s0f0np0": ("SN2100-TOR", f"Eth1/{i}")},
        hostname=f"spark-{i:02d}.rack1.lab", dmi_product="DGX Spark",
    )

# Triangle of /30 cables PLUS a fourth Spark on a shared /24 switch with spark1.
out["mix-spark4"] = spark(
    "spark4", 4,
    {"enp1s0f0np0": "10.30.0.14/24"},
    "192.168.1.24/24", "100.101.1.4/32",
    lldp={"enp1s0f0np0": ("leaf-a", "Ethernet1")},
    hostname="spark-d.lab", dmi_product="DGX Spark",
)
# spark1 also joins that switch fabric (second cable on twin port).
out["mix-spark1"] = spark(
    "spark1", 1,
    {"enp1s0f0np0": "10.10.12.1/30", "enp1s0f1np1": "10.10.13.1/30", "enP2p1s0f0np0": "10.30.0.11/24"},
    "192.168.1.21/24", "100.101.1.1/32",
    lldp={"enP2p1s0f0np0": ("leaf-a", "Ethernet2")},
    hostname="spark-a.lab", dmi_product="DGX Spark",
)

for name, body in out.items():
    with open(os.path.join(HERE, f"{name}.json"), "w") as f:
        json.dump(body, f, indent=1, sort_keys=True)
        f.write("\n")
print("wrote", len(out), "fixtures")
