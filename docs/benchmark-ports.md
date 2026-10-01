# Benchmark: the k9s ports

This test compared sofka before and after four changes taken from k9s: metrics polls paced to the metrics-server sample period, log streams that reconnect, API discovery that runs again for an unknown `:` name, and port-forwarding through the Kubernetes API instead of `kubectl`. It ran on 1 October 2026 against one GKE staging cluster. The results apply to this computer, cluster, and method.

The [test script](../scripts/benchmark-ports.py) and [raw results](benchmarks/ports-2026-10-01.json) are part of this repository.

## Builds

Both builds came from commit `71cbdb5`. The after build adds only the `src/` changes of the four ports, from commits `4e391a2` and `efd1317`. Both used `cargo build --release --locked`.

| Build  | SHA-256 of the executable                                          |
| ------ | ------------------------------------------------------------------ |
| before | `5edfaf7f46cd805efaeb79ba9b8c1b57f094df19c3bb75a390de1d5c39dc7542` |
| after  | `d12a1b2ed1d0ac5f8f630edf9854731984fbc65623a6f313192c8f5d18bf8c33` |

Environment: Apple M4 Pro, macOS 27.0.1, rustc 1.98.1, tmux 3.7c, a 180 by 50 terminal. Cluster: GKE v1.35.8 with 252 pods and metrics-server at `--metric-resolution=30s`.

## Method

Each trial starts a new sofka process in an isolated tmux server, with empty XDG directories. The script reads the screen with `tmux capture-pane`. Trials alternate the build order. Each scenario ran 5 trials per build, except metrics with 4.

API requests are counted outside sofka. sofka connects through `kubectl proxy -v=6`, and the script parses its request log. For the metrics scenario, a second proxy in the script also records the newest sample `timestamp` in each metrics response. Both counts agreed in every trial. Port-forward trials connect directly, because a proxy hop would add latency to the measured path.

The script creates the namespace `sofka-bench` with a logging pod, a web pod that serves a 32 MiB file, and a service. The discovery scenario creates and deletes the CRD `sofkaprobes.bench.sofka.dev`. Run `scripts/benchmark-ports.py --teardown` to delete them.

## Metrics polling

sofka showed the pod view over all namespaces for 240 seconds per trial.

| Measurement                         | before |  after |
| ----------------------------------- | -----: | -----: |
| Metrics requests per minute, median |  11.36 |   4.37 |
| Sample age at arrival, median       |  3.7 s |  2.3 s |
| Sample age at arrival, p95          |  5.9 s |  3.6 s |
| Samples skipped                     |      0 |      0 |
| sofka CPU time per trial, median    | 0.99 s | 0.88 s |

Sample age is the end of the first response that carried a sample, minus the sample's `timestamp`. The timestamp is the kubelet scrape time, so the age includes the metrics-server publishing delay and the clock offset to the cluster. Both are the same for both builds.

## Log reconnect

sofka followed the logs of a pod that prints a numbered line every 0.1 s. The script then made the container exit. The kubelet restarted it after about 2 s.

| Measurement                                     | before |             after |
| ----------------------------------------------- | -----: | ----------------: |
| Trials that showed lines from the new container | 0 of 5 |            5 of 5 |
| First line of the new container shown           |      - | `seq=0` in 5 of 5 |
| Duplicate lines in the saved buffer             |      0 |                 0 |
| Missing sequence numbers in the saved buffer    |      0 |                 0 |
| Log requests per trial                          |      1 |                 2 |

The after build showed the new lines a median of 0.34 s after the script saw the container as running. The script checks the pod status every 0.5 s.

## Discovery of a new CRD

sofka connected first. Then the script installed a CRD and one object, and opened the kind with `:sofkaprobes`. Later it entered two unknown names, more than 10 s after the open.

| Measurement                                  | before    | after          |
| -------------------------------------------- | --------- | -------------- |
| Result of `:sofkaprobes`                     | not found | opened, 5 of 5 |
| Time until the object or the error shows     | 32 ms     | 455 ms         |
| First unknown name, time until the error     | 12 ms     | 198 ms         |
| Second unknown name, time until the error    | 13 ms     | 15 ms          |
| Discovery requests while idle                | 0         | 0              |
| Discovery requests for the two unknown names | 0         | 2              |

Discovery is two requests, `/api` and `/apis`. The second unknown name falls in the 10 s cooldown, so it costs no request.

## Port-forward

Three saved forwards started on connect: one to the pod, one to the service, and one to a port where nothing listens.

| Measurement                                      | before (`kubectl`) | after (API) |
| ------------------------------------------------ | -----------------: | ----------: |
| Launch to first response, pod forward, median    |            1193 ms |      862 ms |
| Launch to first response, service forward        |            1289 ms |     1109 ms |
| New connection plus small GET, median            |              70 ms |      270 ms |
| New connection plus small GET, p95               |             130 ms |      397 ms |
| One 32 MiB download, median                      |         23.4 MiB/s |  22.7 MiB/s |
| Eight parallel 32 MiB downloads                  |         24.3 MiB/s |  24.0 MiB/s |
| Child processes                                  |                  3 |           0 |
| Memory of child processes (RSS)                  |            169 MiB |       0 MiB |
| Memory of sofka (RSS)                            |             25 MiB |      37 MiB |
| Forward still listens after a refused connection |             0 of 5 |      5 of 5 |

`kubectl` keeps one connection to the kubelet and opens a stream on it for each local connection. sofka opens a new API connection (a WebSocket upgrade) for each local connection, so each new connection costs about 200 ms more on this cluster. Throughput after the connection opens is the same. `kubectl port-forward` exits when the pod refuses one connection ("lost connection to pod"). sofka keeps the forward and fails only that connection.

## Limits

- One cluster in one region, and one computer. Network delay to the API server changes the port-forward connection cost.
- The metrics result depends on the metrics-server resolution. With the upstream 15 s, sofka polls about twice per sample, so fewer requests are saved.
- An earlier version of the script polled the metrics API twice per second to time samples. On this cluster, metrics-server has a 63 MiB memory limit and was OOMKilled once during that run. That method was removed. The current script adds no metrics requests.
