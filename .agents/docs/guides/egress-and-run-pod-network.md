# The egress proxy and the network of run pods: evidence and commands

For people: [The egress proxy and the network of run pods](../../../docs/guides/egress-and-run-pod-network.md). That page decides; this one gives the
evidence and the commands. Checked at `257175c` on 2026-11-26. The decision is
[ADR 0042](../../../docs/decisions/0042-run-pods-reach-the-internet-through-an-egress-proxy.md); the values are in
[the chart README](../../../deploy/coder/README.md#the-egress-proxy).

## Commands

```sh
# the proxy's refusals and its decisions: one JSON line each (decision, reason, host, port, client), never a path, query or header
kubectl -n <ns> logs deploy/<fullname>-egress | grep 'egress request refused'
# the three policies and the proxy's objects (the name is the chart's full name, <fullname>, and a suffix)
kubectl -n <ns> get networkpolicy,deploy,svc,cm,pdb | grep -E '<fullname>-(run-pods|egress)'
# the allow-list the proxy runs with: read at start only, so a change is a rollout
kubectl -n <ns> get cm <fullname>-egress-allow -o yaml
# what a render adds, before a roll
helm template <release> deploy/coder -n <ns> -f my-values.yaml | grep -B2 -A12 'kind: NetworkPolicy'
# a direct connection from a run pod must fail, a proxied one must work (the kind job does this: curl --noproxy '*')
kubectl -n <ns> exec <run-pod> -c run -- sh -c 'curl -sS -m 5 --noproxy "*" https://registry.npmjs.org/ ; echo "exit=$?"'
```

The run pods carry `app.kubernetes.io/component: run` (`deploy/coder/templates/_runpods.tpl:169`). `<fullname>` is the chart's full name:
the release name, joined with the chart name unless it already contains it (`deploy/coder/templates/_helpers.tpl:6-14`). `curl` is in the
workspace image the kind job runs (`deploy/coder/tests/kind-run-pods.sh:412`).

## Evidence

| Claim on the page for people | Where it is true |
|---|---|
| the template sets `HTTPS_PROXY`, `HTTP_PROXY`, lower-case twins and `NO_PROXY` | `deploy/coder/templates/_runpods.tpl:237-241`; `NO_PROXY` is `localhost,127.0.0.1` and `extraNoProxy` (`:143-145`) |
| `allow` then `extraAllow`, lower-cased, once each | `deploy/coder/templates/_runpods.tpl:134-140` |
| the chart refuses a wildcard, a URL, a port, an IP address, a bare suffix and an empty list | `deploy/coder/templates/_validate.tpl:445-463` |
| `runPods.networkPolicy.enabled: false` is refused | `deploy/coder/templates/_validate.tpl:417` |
| the proxy answers `403` to anything but `CONNECT`, to an IP literal, a port or a name not allowed, and `no public address` | `bin/adam-egress/README.md` "What a request gets"; `bin/adam-egress/src/server.rs` |
| 8 KiB head, 10 s to read, resolve and connect, four addresses, 25 s drain, 300 s idle | `bin/adam-egress/README.md` "Variables" (`Limits`, `src/config.rs`) |
| one log line per decision, no path, query or header | `bin/adam-egress/README.md` "Logs" |
| the idle sweep is `runPods.idleSecs` (900) and the coder waits `RUN_POD_WAIT_SECS` | `deploy/coder/templates/statefulset.yaml:367-370`; chart README "Run pods" |
| the proxy's pods get TCP 443 only, except the blocked and cluster ranges | chart README "The egress proxy"; ADR 0042 decision 5 |
| no policy names an IPv6 range | `deploy/coder/tests/render-check.sh` (asserted on the three policies); ADR 0042 decision 6 |
| kindnet enforces the policies and the job fails where they are not | `deploy/coder/tests/kind-run-pods.sh`, `ADAM_TEST_CNI_MAY_IGNORE_POLICIES`; ADR 0042 "Facts" (*verified 2026-10-27*, run 40435825771, as the ADR says) |

## Unverified

* That the cluster's own network plugin enforces `NetworkPolicy`: only kind's was exercised.
* That git, cargo, pip and go read the proxy variables (the kind job tries curl and npm).
* What the chart README's "Verified and unverified" lists for run pods: the Kubernetes version (1.37 or later, the chart's floor, ADR 0080), Longhorn sharing a `ReadWriteOnce` volume, the pod and service CIDRs, and the HTTP status of an admission refusal.
