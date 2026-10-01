#!/usr/bin/env python3
"""Compare two sofka builds on the k9s ports: metrics polling, log reconnect,
rediscovery of new CRDs, and port-forwarding.

The script writes to the cluster. It creates the namespace `sofka-bench`,
two pods and a service in it, and the cluster-scoped CRD
`sofkaprobes.bench.sofka.dev`. `--teardown` deletes all of them.

Trials alternate the build order. Request counts come from a `kubectl proxy
-v=6` log, not from sofka, so each build is measured by the same outside
observer. Port-forward trials connect directly, because the proxy hop would
add latency to the thing being measured.
"""

import argparse
import concurrent.futures
import datetime
import hashlib
import gzip
import http.client
import http.server
import json
import os
import platform
import re
import shlex
import socket
import statistics
import subprocess
import tempfile
import threading
import time
import urllib.request
from pathlib import Path

NS = "sofka-bench"
CRD = "sofkaprobes.bench.sofka.dev"
PROXY_PORT = 18001
INSPECT_PORT = 18002
PF_POD_PORT = 18080
PF_SVC_PORT = 18081
PF_REFUSED_PORT = 18082
BIG_BYTES = 33554432

FIXTURES = f"""
apiVersion: v1
kind: Namespace
metadata:
  name: {NS}
  labels: {{purpose: sofka-benchmark}}
---
apiVersion: v1
kind: Pod
metadata:
  name: logger
  namespace: {NS}
  labels: {{app: logger}}
spec:
  restartPolicy: Always
  terminationGracePeriodSeconds: 1
  containers:
  - name: logger
    image: busybox:1.36
    command: ["sh", "-c", "boot=$(date +%s)-$RANDOM; i=0; while [ ! -f /tmp/stop ]; do echo \\"boot=$boot seq=$i\\"; i=$((i+1)); sleep 0.1; done; echo \\"boot=$boot end\\"; exit 0"]
    resources: {{requests: {{cpu: 10m, memory: 16Mi}}, limits: {{memory: 32Mi}}}}
---
apiVersion: v1
kind: Pod
metadata:
  name: web
  namespace: {NS}
  labels: {{app: web}}
spec:
  terminationGracePeriodSeconds: 1
  containers:
  - name: web
    image: busybox:1.36
    command: ["sh", "-c", "mkdir -p /www && echo ok > /www/small && head -c {BIG_BYTES} /dev/urandom > /www/big && exec httpd -f -p 8080 -h /www"]
    ports: [{{name: http, containerPort: 8080}}]
    readinessProbe: {{tcpSocket: {{port: 8080}}, periodSeconds: 2}}
    resources: {{requests: {{cpu: 50m, memory: 64Mi}}, limits: {{memory: 128Mi}}}}
---
apiVersion: v1
kind: Service
metadata:
  name: web
  namespace: {NS}
spec:
  selector: {{app: web}}
  ports: [{{name: http, port: 80, targetPort: http}}]
"""

CRD_YAML = """
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata:
  name: sofkaprobes.bench.sofka.dev
spec:
  group: bench.sofka.dev
  scope: Namespaced
  names: {plural: sofkaprobes, singular: sofkaprobe, kind: SofkaProbe}
  versions:
  - name: v1
    served: true
    storage: true
    schema:
      openAPIV3Schema: {type: object, x-kubernetes-preserve-unknown-fields: true}
"""

CR_YAML = f"""
apiVersion: bench.sofka.dev/v1
kind: SofkaProbe
metadata: {{name: probe-one, namespace: {NS}}}
"""

PROXY_KUBECONFIG = f"""apiVersion: v1
kind: Config
clusters:
- name: proxy
  cluster: {{server: "http://127.0.0.1:{PROXY_PORT}"}}
contexts:
- name: CONTEXT
  context: {{cluster: proxy, user: none, namespace: default}}
users:
- name: none
  user: {{}}
current-context: CONTEXT
"""

RESPONSE = re.compile(
    r'^I\d{4} (\d\d:\d\d:\d\d\.\d+)\s.*"Response" verb="(\w+)" '
    r'url="([^"]+)" status="([^"]*)" milliseconds=(\d+)'
)


def run(args, **kwargs):
    return subprocess.run(
        args, check=True, text=True, capture_output=True, **kwargs
    ).stdout


def stats(values):
    values = sorted(values)
    if not values:
        return None
    rank = (len(values) - 1) * 0.95
    lo = int(rank)
    return dict(
        n=len(values),
        median=statistics.median(values),
        minimum=values[0],
        maximum=values[-1],
        p95=values[lo]
        + (values[min(lo + 1, len(values) - 1)] - values[lo]) * (rank - lo),
    )


class Inspector(http.server.ThreadingHTTPServer):
    """A reverse proxy in front of `kubectl proxy` that records which metrics
    sample each response carried. Other responses stream through unchanged."""

    daemon_threads = True

    def __init__(self):
        self.log = []
        self.lock = threading.Lock()
        super().__init__(("127.0.0.1", INSPECT_PORT), InspectHandler)


class InspectHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_GET(self):
        start = time.time()
        upstream = http.client.HTTPConnection("127.0.0.1", PROXY_PORT, timeout=3600)
        headers = {k: v for k, v in self.headers.items() if k.lower() != "host"}
        upstream.request("GET", self.path, headers=headers)
        resp = upstream.getresponse()
        metrics = self.path.startswith("/apis/metrics.k8s.io/") and "/pods" in self.path
        self.send_response(resp.status)
        for k, v in resp.getheaders():
            if k.lower() not in ("transfer-encoding", "content-length", "connection"):
                self.send_header(k, v)
        if metrics:
            body = resp.read()
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            newest = None
            try:
                data = body
                if resp.getheader("Content-Encoding", "") == "gzip":
                    data = gzip.decompress(body)
                newest = max(i["timestamp"] for i in json.loads(data)["items"])
            except (ValueError, KeyError, OSError):
                pass
            with self.server.lock:
                self.server.log.append(dict(start=start, end=time.time(), newest=newest))
            upstream.close()
            return
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()
        try:
            while True:
                chunk = resp.read1(65536)
                if not chunk:
                    break
                self.wfile.write(b"%x\r\n%s\r\n" % (len(chunk), chunk))
                self.wfile.flush()
            self.wfile.write(b"0\r\n\r\n")
        except (BrokenPipeError, ConnectionResetError):
            pass
        finally:
            upstream.close()
            self.close_connection = True


class Bench:
    def __init__(self, args, root):
        self.args = args
        self.root = Path(root)
        self.kubectl = ["kubectl", "--context", args.context, "--request-timeout=30s"]
        self.socket = f"sofka-ports-{os.getpid()}"
        self.base_env = {
            k: v for k, v in os.environ.items() if not k.startswith(("SOFKA_", "K9S_"))
        }
        self.base_env["TERM"] = "xterm-256color"
        self.proxy = None
        self.trial_id = 0

    # tmux -----------------------------------------------------------------
    def tmux(self, *parts):
        return run(["tmux", "-L", self.socket, "-f", "/dev/null", *parts])

    def screen(self):
        return self.tmux("capture-pane", "-p", "-t", "bench:0.0")

    def keys(self, *keys, literal=False):
        self.tmux("send-keys", "-t", "bench:0.0", *(["-l"] if literal else []), *keys)

    def wait_for(self, predicate, start, timeout=60):
        while time.perf_counter() - start < timeout:
            text = self.screen()
            if predicate(text):
                return (time.perf_counter() - start) * 1000
            time.sleep(0.02)
        raise TimeoutError("screen condition not reached")

    def launch(self, binary, argv, proxied, config=None):
        self.trial_id += 1
        trial = self.root / f"trial-{self.trial_id}"
        env = self.base_env.copy()
        for key in [
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
            "TMPDIR",
        ]:
            env[key] = str(trial / key)
            Path(env[key]).mkdir(parents=True)
        if config:
            (trial / "XDG_CONFIG_HOME" / "sofka").mkdir()
            (trial / "XDG_CONFIG_HOME" / "sofka" / "config.toml").write_text(config)
        if proxied:
            name = "inspect" if proxied == "inspect" else "proxy"
            env["KUBECONFIG"] = str(self.root / f"{name}.kubeconfig")
        command = [binary, "--context", self.args.context, *argv]
        launch = ["env", "-i"] + [f"{k}={v}" for k, v in env.items()] + command
        self.tmux("respawn-pane", "-k", "-t", "bench:0.0", "exec " + shlex.join(launch))
        return trial

    def pane_pid(self):
        return int(
            self.tmux("display-message", "-p", "-t", "bench:0.0", "#{pane_pid}").strip()
        )

    def quit(self):
        self.keys("C-c")
        deadline = time.time() + 5
        while time.time() < deadline:
            if (
                self.tmux(
                    "display-message", "-p", "-t", "bench:0.0", "#{pane_dead}"
                ).strip()
                == "1"
            ):
                return
            time.sleep(0.05)

    # proxy ----------------------------------------------------------------
    def start_proxy(self):
        (self.root / "proxy.kubeconfig").write_text(
            PROXY_KUBECONFIG.replace("CONTEXT", self.args.context)
        )
        (self.root / "inspect.kubeconfig").write_text(
            PROXY_KUBECONFIG.replace("CONTEXT", self.args.context).replace(
                str(PROXY_PORT), str(INSPECT_PORT)
            )
        )
        self.inspector = Inspector()
        threading.Thread(target=self.inspector.serve_forever, daemon=True).start()
        self.proxy_log = self.root / "proxy.log"
        self.proxy = subprocess.Popen(
            self.kubectl[:3]
            + ["proxy", f"--port={PROXY_PORT}", "--reject-paths=^$", "-v=6"],
            stdout=open(self.proxy_log, "w"),
            stderr=subprocess.STDOUT,
        )
        deadline = time.time() + 10
        while time.time() < deadline:
            try:
                urllib.request.urlopen(
                    f"http://127.0.0.1:{PROXY_PORT}/version", timeout=2
                ).read()
                return
            except OSError:
                time.sleep(0.2)
        raise RuntimeError("kubectl proxy did not start")

    def requests_since(self, start_wall, end_wall):
        """Proxied API requests between two wall-clock times: (start, end, verb, path)."""
        out = []
        day = datetime.date.today()
        for line in self.proxy_log.read_text().splitlines():
            m = RESPONSE.match(line)
            if not m:
                continue
            end = datetime.datetime.combine(
                day, datetime.time.fromisoformat(m[1])
            ).timestamp()
            if not (start_wall <= end <= end_wall):
                continue
            path = re.sub(r"^https?://[^/]+", "", m[3])
            out.append(
                dict(
                    start=end - int(m[5]) / 1000,
                    end=end,
                    verb=m[2],
                    path=path,
                    status=m[4],
                )
            )
        return out

    # fixtures -------------------------------------------------------------
    def fresh_logger(self):
        # A new pod per trial keeps the kubelet restart backoff at its first step.
        run(self.kubectl + ["-n", NS, "delete", "pod", "logger", "--ignore-not-found", "--wait=true"])
        subprocess.run(
            self.kubectl + ["apply", "-f", "-"],
            input=FIXTURES,
            text=True,
            check=True,
            capture_output=True,
        )
        run(self.kubectl + ["-n", NS, "wait", "--for=condition=Ready", "pod/logger", "--timeout=180s"])

    def setup(self):
        run(self.kubectl + ["-n", NS, "delete", "pod", "logger", "--ignore-not-found", "--wait=true"])
        subprocess.run(
            self.kubectl + ["apply", "-f", "-"],
            input=FIXTURES,
            text=True,
            check=True,
            capture_output=True,
        )
        run(
            self.kubectl
            + [
                "-n",
                NS,
                "wait",
                "--for=condition=Ready",
                "pod/logger",
                "pod/web",
                "--timeout=180s",
            ]
        )
        self.delete_crd()

    def teardown(self):
        self.delete_crd()
        subprocess.run(
            self.kubectl
            + ["delete", "namespace", NS, "--ignore-not-found", "--wait=false"],
            check=True,
            capture_output=True,
        )

    def delete_crd(self):
        subprocess.run(
            self.kubectl
            + [
                "delete",
                "crd",
                CRD,
                "--ignore-not-found",
                "--wait=true",
                "--timeout=120s",
            ],
            check=True,
            capture_output=True,
        )

    # scenarios ------------------------------------------------------------
    def metrics(self, name, binary):
        """Pod view over all namespaces for a fixed window. Counts metrics requests
        and measures how old each new metrics-server sample was when it reached
        sofka.

        An inspecting proxy records the newest sample `timestamp` in every
        metrics response. The age of a sample is the end of the first response
        that carried it minus that timestamp. The timestamp is the kubelet
        scrape time, so the age includes the metrics-server publishing delay and
        the clock offset to the cluster. Both are the same for both builds. The
        measurement adds no requests to the cluster.
        """
        window = self.args.metrics_seconds
        with self.inspector.lock:
            self.inspector.log.clear()
        start_wall = time.time()
        start = time.perf_counter()
        self.launch(binary, ["--readonly", "-A", "pods"], proxied="inspect")
        row = dict(scenario="metrics", program=name)
        try:
            row["startup_ms"] = self.wait_for(
                lambda s: re.search(r"\bpods \[\d", s), start
            )
            pid = self.pane_pid()
            cpu0 = cpu_seconds(pid)
            time.sleep(window)
            row["cpu_seconds"] = cpu_seconds(pid) - cpu0
        finally:
            end_wall = time.time()
            self.quit()
        with self.inspector.lock:
            responses = list(self.inspector.log)
        counted = [
            r
            for r in self.requests_since(start_wall, end_wall + 1)
            if r["path"].startswith("/apis/metrics.k8s.io/") and "/pods" in r["path"]
        ]
        minutes = (end_wall - start_wall) / 60
        row["metrics_requests"] = len(responses)
        row["metrics_requests_kubectl_log"] = len(counted)
        row["metrics_requests_per_min"] = len(responses) / minutes
        row["window_s"] = end_wall - start_wall
        row["unparsed_responses"] = sum(1 for r in responses if not r["newest"])
        first = {}
        for r in sorted(responses, key=lambda r: r["end"]):
            if r["newest"]:
                first.setdefault(r["newest"], r["end"])
        # The sample in the first response predates sofka; it has no delivery age.
        samples = sorted(first.items())[1:]
        epoch = lambda ts: datetime.datetime.fromisoformat(ts.replace("Z", "+00:00")).timestamp()
        row["sample_age_s"] = [end - epoch(ts) for ts, end in samples]
        stamps = [epoch(ts) for ts in sorted(first)]
        gaps = [b - a for a, b in zip(stamps, stamps[1:])]
        period = min(gaps) if gaps else 0
        row["samples_seen"] = len(first)
        row["skipped_samples"] = sum(round(g / period) - 1 for g in gaps) if period else 0
        row["sample_gaps_s"] = gaps
        return row

    def logs(self, name, binary):
        """Follow the logger pod, then make its container exit. Measures whether and
        when lines from the restarted container reach the view, and saves the full
        buffer to check for gaps and duplicates."""
        self.fresh_logger()
        start_wall = time.time()
        start = time.perf_counter()
        trial = self.launch(binary, ["-n", NS, "pods"], proxied=True)
        row = dict(scenario="logs", program=name)
        try:
            self.wait_for(
                lambda s: re.search(r"\blogger\s+1/1\s+Running", s), start, 90
            )
            self.keys("/")
            self.keys("logger", literal=True)
            self.keys("Enter")
            time.sleep(0.5)
            self.keys("l")
            self.wait_for(lambda s: "boot=" in s, time.perf_counter())
            old_boot = re.search(r"boot=(\S+) seq=", self.screen())[1]
            restarts = int(
                run(
                    self.kubectl
                    + [
                        "-n",
                        NS,
                        "get",
                        "pod",
                        "logger",
                        "-o",
                        "jsonpath={.status.containerStatuses[0].restartCount}",
                    ]
                )
            )
            time.sleep(2)
            run(self.kubectl + ["-n", NS, "exec", "logger", "--", "touch", "/tmp/stop"])
            stopped = time.perf_counter()
            # The new container is "started" once the kubelet reports it running.
            restarted = None
            while time.perf_counter() - stopped < 120:
                status = json.loads(
                    run(self.kubectl + ["-n", NS, "get", "pod", "logger", "-o", "json"])
                )
                cs = status["status"]["containerStatuses"][0]
                if cs["restartCount"] > restarts and "running" in cs["state"]:
                    restarted = time.perf_counter()
                    break
                time.sleep(0.5)
            row["container_restart_s"] = (
                None if restarted is None else restarted - stopped
            )
            new_line = lambda s: any(
                m != old_boot for m in re.findall(r"boot=(\S+) seq=", s)
            )
            try:
                self.wait_for(new_line, restarted or stopped, self.args.log_wait)
                row["new_container_visible"] = True
                row["visible_after_restart_s"] = time.perf_counter() - (
                    restarted or stopped
                )
                row["visible_after_stop_s"] = time.perf_counter() - stopped
            except TimeoutError:
                row["new_container_visible"] = False
            time.sleep(3)
            self.keys("C-s")
            saved = None
            deadline = time.time() + 10
            while time.time() < deadline and saved is None:
                found = list((trial / "TMPDIR").glob("sofka-*.log"))
                saved = found[0] if found else None
                time.sleep(0.2)
            if saved:
                time.sleep(0.5)
                row.update(analyze_log(saved.read_text(), old_boot))
        finally:
            end_wall = time.time()
            self.quit()
        reqs = self.requests_since(start_wall, end_wall + 1)
        row["log_requests"] = sum(
            1 for r in reqs if r["path"].split("?")[0].endswith("/log")
        )
        row["pod_get_requests"] = sum(
            1 for r in reqs if r["path"].split("?")[0].endswith("/pods/logger")
        )
        return row

    def discovery(self, name, binary):
        """Install a CRD after sofka connects, then open it with `:`."""
        self.delete_crd()
        start_wall = time.time()
        start = time.perf_counter()
        self.launch(binary, ["--readonly", "-n", NS, "pods"], proxied=True)
        row = dict(scenario="discovery", program=name)
        try:
            self.wait_for(lambda s: re.search(r"\bpods \[\d", s), start, 90)
            boot_end = time.time()
            subprocess.run(
                self.kubectl + ["apply", "-f", "-"],
                input=CRD_YAML,
                text=True,
                check=True,
                capture_output=True,
            )
            run(
                self.kubectl
                + ["wait", "--for=condition=Established", f"crd/{CRD}", "--timeout=60s"]
            )
            deadline = time.time() + 60
            while True:
                try:
                    subprocess.run(
                        self.kubectl + ["apply", "-f", "-"],
                        input=CR_YAML,
                        text=True,
                        check=True,
                        capture_output=True,
                    )
                    break
                except subprocess.CalledProcessError:
                    if time.time() > deadline:
                        raise
                    time.sleep(0.5)
            # Wait until aggregated discovery on the server lists the new group.
            deadline = time.time() + 60
            while "bench.sofka.dev" not in run(
                self.kubectl + ["get", "--raw", "/apis"]
            ):
                if time.time() > deadline:
                    raise TimeoutError("server discovery did not list the CRD")
                time.sleep(0.5)
            open_wall = time.time()
            t = time.perf_counter()
            self.keys(":")
            self.keys("sofkaprobes", literal=True)
            self.keys("Enter")
            outcome = {}

            def done(s):
                if re.search(r"\bprobe-one\b", s):
                    outcome["result"] = "opened"
                    return True
                if "No resource matches" in s:
                    outcome["result"] = "not found"
                    return True
                return False

            row["open_ms"] = self.wait_for(done, t, 30)
            row["result"] = outcome["result"]
            open_end = time.time()
            # Two typos in a row, after the 10 s rediscovery cooldown of the
            # build under test has expired. Each name is new, so a stale flash
            # cannot satisfy the wait.
            time.sleep(11)
            miss_wall = time.time()
            for n, typo in enumerate(("zzqqxxnokinda", "zzqqxxnokindb"), 1):
                self.keys(":")
                self.keys(typo, literal=True)
                t = time.perf_counter()
                self.keys("Enter")
                row[f"miss{n}_ms"] = self.wait_for(
                    lambda s: f"No resource matches '{typo}'" in s, t, 30
                )
                time.sleep(0.3)
            miss_end = time.time()
        finally:
            end_wall = time.time()
            self.quit()
        reqs = self.requests_since(start_wall, end_wall + 1)

        def disc(lo, hi):
            return sum(
                1
                for r in reqs
                if lo <= r["end"] <= hi and r["path"].split("?")[0] in ("/api", "/apis")
            )

        row["discovery_requests_startup"] = disc(start_wall, boot_end)
        row["discovery_requests_idle"] = disc(boot_end, open_wall)
        row["discovery_requests_open"] = disc(open_wall, open_end + 0.5)
        row["discovery_requests_two_misses"] = disc(miss_wall, miss_end + 0.5)
        self.delete_crd()
        return row

    def portforward(self, name, binary):
        """Two saved forwards (pod and service) started on connect."""
        config = (
            f'[[forwards]]\nname = "pod"\ntarget = "pod/web"\nnamespace = "{NS}"\n'
            f'ports = "{PF_POD_PORT}:8080"\nautostart = true\n\n'
            f'[[forwards]]\nname = "svc"\ntarget = "svc/web"\nnamespace = "{NS}"\n'
            f'ports = "{PF_SVC_PORT}:80"\nautostart = true\n\n'
            # Nothing listens on 9999, so every connection is refused in the pod.
            f'[[forwards]]\nname = "refused"\ntarget = "pod/web"\nnamespace = "{NS}"\n'
            f'ports = "{PF_REFUSED_PORT}:9999"\nautostart = true\n'
        )
        for port in (PF_POD_PORT, PF_SVC_PORT, PF_REFUSED_PORT):
            if port_open(port):
                raise RuntimeError(f"port {port} is in use before the trial")
        start = time.perf_counter()
        self.launch(binary, ["-n", NS, "pods"], proxied=False, config=config)
        row = dict(scenario="portforward", program=name)
        try:
            for label, port in (("pod", PF_POD_PORT), ("svc", PF_SVC_PORT)):
                row[f"{label}_first_response_ms"] = first_response(port, start)
            for label, port in (("pod", PF_POD_PORT), ("svc", PF_SVC_PORT)):
                row[f"{label}_connect_get_ms"] = [
                    timed_get(port, "/small")[0] for _ in range(30)
                ]
            row["throughput_mib_s"] = []
            for _ in range(3):
                ms, size = timed_get(PF_POD_PORT, "/big")
                assert size == BIG_BYTES, size
                row["throughput_mib_s"].append(size / 1048576 / (ms / 1000))
            t = time.perf_counter()
            with concurrent.futures.ThreadPoolExecutor(8) as pool:
                sizes = list(
                    pool.map(lambda _: timed_get(PF_POD_PORT, "/big")[1], range(8))
                )
            row["parallel8_ok"] = sum(s == BIG_BYTES for s in sizes)
            row["parallel8_mib_s"] = sum(sizes) / 1048576 / (time.perf_counter() - t)
            pid = self.pane_pid()
            children = process_tree(pid)
            row["child_processes"] = len(children)
            row["child_rss_mib"] = sum(rss_mib(p) for p in children)
            row["sofka_rss_mib"] = rss_mib(pid)
            # One connection the pod refuses, then check the forward still listens.
            try:
                timed_get(PF_REFUSED_PORT, "/")
            except OSError:
                pass
            time.sleep(3)
            row["survives_refused_connection"] = port_open(PF_REFUSED_PORT)
        finally:
            self.quit()
        time.sleep(1)
        row["ports_closed_after_quit"] = not any(
            port_open(p) for p in (PF_POD_PORT, PF_SVC_PORT, PF_REFUSED_PORT)
        )
        return row


def analyze_log(text, old_boot):
    by_boot = {}
    lines = [re.search(r"boot=(\S+) (seq=(\d+)|end)", l) for l in text.splitlines()]
    dupes = 0
    seen = set()
    for m in lines:
        if not m:
            continue
        key = m[0]
        if key in seen:
            dupes += 1
        seen.add(key)
        if m[3] is not None:
            by_boot.setdefault(m[1], []).append(int(m[3]))
    new = [b for b in by_boot if b != old_boot]
    out = dict(
        saved_lines=len(text.splitlines()),
        duplicate_lines=dupes,
        old_boot_end_seen=f"boot={old_boot} end" in text,
    )
    gaps = 0
    for seqs in by_boot.values():
        ordered = sorted(seqs)
        gaps += sum(b - a - 1 for a, b in zip(ordered, ordered[1:]) if b > a + 1)
    out["sequence_gaps"] = gaps
    if new:
        out["new_boot_first_seq"] = min(by_boot[new[0]])
        out["new_boot_lines"] = len(by_boot[new[0]])
    return out


def cpu_seconds(pid):
    text = run(["/bin/ps", "-o", "time=", "-p", str(pid)]).strip()
    parts = [float(p) for p in text.replace("-", ":").split(":")]
    total = 0.0
    for p in parts:
        total = total * 60 + p
    return total


def rss_mib(pid):
    try:
        return float(run(["/bin/ps", "-o", "rss=", "-p", str(pid)])) / 1024
    except subprocess.CalledProcessError:
        return 0.0


def process_tree(pid):
    out = []
    try:
        kids = [int(p) for p in run(["pgrep", "-P", str(pid)]).split()]
    except subprocess.CalledProcessError:
        return out
    for k in kids:
        out.append(k)
        out.extend(process_tree(k))
    return out


def port_open(port):
    with socket.socket() as s:
        s.settimeout(0.2)
        return s.connect_ex(("127.0.0.1", port)) == 0


def timed_get(port, path):
    t = time.perf_counter()
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}{path}", headers={"Connection": "close"}
    )
    with urllib.request.urlopen(req, timeout=60) as resp:
        size = len(resp.read())
    return (time.perf_counter() - t) * 1000, size


def first_response(port, start, timeout=60):
    while time.perf_counter() - start < timeout:
        try:
            timed_get(port, "/small")
            return (time.perf_counter() - start) * 1000
        except OSError:
            time.sleep(0.02)
    raise TimeoutError(f"port {port} never answered")


def summarize(runs):
    keys = [
        "startup_ms",
        "cpu_seconds",
        "metrics_requests_per_min",
        "metrics_requests_kubectl_log",
        "sample_age_s",
        "skipped_samples",
        "unparsed_responses",
        "visible_after_restart_s",
        "visible_after_stop_s",
        "container_restart_s",
        "log_requests",
        "pod_get_requests",
        "duplicate_lines",
        "sequence_gaps",
        "new_boot_first_seq",
        "open_ms",
        "miss1_ms",
        "miss2_ms",
        "discovery_requests_startup",
        "discovery_requests_idle",
        "discovery_requests_open",
        "discovery_requests_two_misses",
        "pod_first_response_ms",
        "svc_first_response_ms",
        "pod_connect_get_ms",
        "svc_connect_get_ms",
        "throughput_mib_s",
        "parallel8_mib_s",
        "parallel8_ok",
        "child_processes",
        "child_rss_mib",
        "sofka_rss_mib",
    ]
    out = {}
    for row in runs:
        group = out.setdefault(row["scenario"], {}).setdefault(row["program"], {})
        for key in keys:
            if key in row and row[key] is not None:
                value = row[key]
                group.setdefault(key, []).extend(
                    value if isinstance(value, list) else [value]
                )
        for key in [
            "result",
            "new_container_visible",
            "ports_closed_after_quit",
            "survives_refused_connection",
            "error",
        ]:
            if key in row:
                group.setdefault(key, []).append(row[key])
    for scenario in out.values():
        for group in scenario.values():
            for key, values in group.items():
                if values and all(
                    isinstance(v, (int, float)) and not isinstance(v, bool)
                    for v in values
                ):
                    group[key] = stats(values)
    return out


def main():
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument("--context", required=True)
    ap.add_argument("--before", required=True, help="sofka binary without the ports")
    ap.add_argument("--after", required=True, help="sofka binary with the ports")
    ap.add_argument("--scenarios", default="metrics,logs,discovery,portforward")
    ap.add_argument(
        "--trials", type=int, default=3, help="trials per build and scenario"
    )
    ap.add_argument("--metrics-seconds", type=int, default=120)
    ap.add_argument("--log-wait", type=int, default=60)
    ap.add_argument("--output", required=True)
    ap.add_argument(
        "--teardown", action="store_true", help="delete the fixtures and exit"
    )
    args = ap.parse_args()
    binaries = {
        "before": str(Path(args.before).resolve()),
        "after": str(Path(args.after).resolve()),
    }
    with tempfile.TemporaryDirectory(prefix="sofka-ports-") as root:
        bench = Bench(args, root)
        if args.teardown:
            bench.teardown()
            return
        result = dict(
            date_utc=time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            platform=platform.platform(),
            context=args.context,
            pods_all_namespaces=len(
                json.loads(run(bench.kubectl + ["get", "pods", "-A", "-o", "json"]))[
                    "items"
                ]
            ),
            server=json.loads(run(bench.kubectl + ["version", "-o", "json"]))[
                "serverVersion"
            ]["gitVersion"],
            binaries={
                n: dict(
                    sha256=hashlib.sha256(Path(b).read_bytes()).hexdigest(),
                    version=run([b, "--version"]).strip(),
                )
                for n, b in binaries.items()
            },
            runs=[],
        )
        output = Path(args.output)
        output.parent.mkdir(parents=True, exist_ok=True)

        def save():
            result["summary"] = summarize(result["runs"])
            output.write_text(json.dumps(result, indent=2) + "\n")

        bench.setup()
        bench.start_proxy()
        try:
            bench.tmux(
                "new-session",
                "-d",
                "-s",
                "bench",
                "-x",
                "180",
                "-y",
                "50",
                "sleep 3600",
            )
            bench.tmux("set-option", "-t", "bench", "remain-on-exit", "on")
            for scenario in args.scenarios.split(","):
                for trial in range(args.trials):
                    order = (
                        ["before", "after"] if trial % 2 == 0 else ["after", "before"]
                    )
                    for name in order:
                        try:
                            row = getattr(bench, scenario)(name, binaries[name])
                        except (
                            TimeoutError,
                            subprocess.CalledProcessError,
                            RuntimeError,
                            AssertionError,
                        ) as exc:
                            row = dict(
                                scenario=scenario,
                                program=name,
                                error=f"{type(exc).__name__}: {exc}",
                            )
                        row["trial"] = trial + 1
                        result["runs"].append(row)
                        save()
                        print(
                            json.dumps(
                                {
                                    k: v
                                    for k, v in row.items()
                                    if not isinstance(v, list)
                                }
                            ),
                            flush=True,
                        )
        finally:
            subprocess.run(
                ["tmux", "-L", bench.socket, "kill-server"], capture_output=True
            )
            if bench.proxy:
                bench.proxy.terminate()
                bench.inspector.shutdown()
            save()
    print(json.dumps(result["summary"], indent=2))


if __name__ == "__main__":
    main()
