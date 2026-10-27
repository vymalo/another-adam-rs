# 0042. Run pods reach the internet through an egress proxy

Status: **Accepted** (2026-10-27), the design of [#124](https://github.com/vymalo/another-adam-rs/issues/124): a run pod's
network is deny by default, and an HTTPS-only proxy of our own, `adam-egress`, is the one way out. Built in two changes: the
proxy (`bin/adam-egress`, in the coder image, [#134](https://github.com/vymalo/another-adam-rs/pull/134)) and this one (the chart,
the policies, the template's variables, the checks, the docs). Amends [ADR 0019](0019-a-runs-processes-in-a-pod-of-their-own.md)
(decision 7's network policy, OD-K6 and the consequence about the policy) and builds on
[ADR 0041](0041-a-run-pod-sees-only-its-own-run.md) (what a run pod holds and sees). What is verified and what is not is marked
in *Facts, and how they are known*; the choices the issue left open are at the end.

## Context

ADR 0019 gave a run pod a `NetworkPolicy` that let it reach the whole internet except the private ranges (OD-K6, "the
internet except the private ranges and the CIDRs named"), so that a build could fetch from anywhere. A run pod now holds more
than a build needs: the repository it works on (ADR 0041 narrowed it to the run's own), and the model key as a variable that
every command of the run can read (OD-K5). A command that a prompt or a dependency's install script made hostile can send all
of it to any public host, and fetch anything from one. The coder is to be offered to every user.

A `NetworkPolicy` names addresses and ports, not hosts: the part of the Kubernetes API that names a host is a plugin's own
(`CiliumNetworkPolicy`'s FQDN rules, a Calico enterprise feature), so a chart that needs to work behind any plugin cannot
use it. What a run needs from the internet is small and known: package registries and git hosts, all over HTTPS. Run pods are
not root, so `apt` is out, and plain HTTP buys nothing.

## Decision

1. **A run pod's egress is deny by default.** Its `NetworkPolicy` (`coder-run-pods` for the release `coder`) allows DNS (as before), the
   egress proxy's pods on TCP 3128, and `runPods.networkPolicy.extraEgress` (an in-cluster target the proxy cannot serve, such as
   a model gateway), and nothing else. The rule that opened `0.0.0.0/0` is gone. **There is no value that brings it back**:
   `runPods.networkPolicy.enabled`, which would have rendered no policy at all, is refused when it is `false` (the render
   fails and says what to use instead). `extraEgress` stays, for what a deployment must add.
2. **The one way out is a proxy of our own, `adam-egress`** ([`bin/adam-egress`](../../bin/adam-egress/README.md)): it accepts
   `CONNECT host:443` to the hosts on an allow-list and nothing else, resolves the name itself, connects only to public IPv4
   addresses, and splices the bytes. No TLS interception, no credential, no plain HTTP; one log line per decision (allowed or
   refused, the host, the port, the reason) and never a path, a query or a header. It is a few hundred lines with the resolver
   and the forbidden ranges injectable, so every rule has a unit test, including a name that resolves to a private address
   (DNS rebinding). **Squid** would need a third-party image pinned by tag and digest and a non-root setup; **Smokescreen** has
   no published image. The proxy runs **the coder's image**, so there is no new image to pin.
3. **The chart deploys it with the run pods** (`runPods.egressProxy`, rendered whenever `runPods.enabled` is, on the roles that
   run workers): a Deployment of `image.repository:image.tag` with the command `tini -- adam-egress` (two replicas, a rolling
   update that never takes the last one, a preStop sleep and a grace period that cover the 25 s the proxy drains its tunnels), a
   `ClusterIP` Service on 3128, a `PodDisruptionBudget` when there is more than one replica, a ServiceAccount of its own with no
   token, and a ConfigMap with the allow-list (a checksum annotation: the file is read at startup only, so a change is a
   rollout). The pods run as uid 10001, non-root, with a read-only root file system, no privilege escalation, every capability
   dropped, the runtime's default seccomp profile and no token. An allow-list that would be empty is refused at render, as
   `adam-egress` would exit 78; so is an entry that is not a host name (a wildcard, a URL, a port, an IP address) or is a bare
   suffix (`.com`).
4. **The run pod's environment names the proxy.** The template (`coder.runPodTemplate`) sets `HTTPS_PROXY`, `https_proxy`,
   `HTTP_PROXY` and `http_proxy` to `http://<name>.<namespace>.svc:3128`, where `<name>` is the proxy's Service (`coder-egress` for the release `coder`: the chart's full name and `-egress`) and `NO_PROXY` and `no_proxy` to
   `localhost,127.0.0.1` (plus `runPods.egressProxy.extraNoProxy`, the hosts of an `extraEgress` target, which the tools must
   not send to the proxy). The commands of a run are `exec`'d into the container, so they inherit the container's environment;
   `runPods.extraEnv` may not set these names. **A tool that ignores the variables gets no connection at all**, because the
   policy denies a direct one: the failure is closed, and it is loud (a timeout) rather than a leak.
5. **The proxy's own policy** (named like the proxy): ingress only from the run pods (the same selector as theirs) on 3128;
   egress to DNS and to `0.0.0.0/0` **on TCP 443 only** (`runPods.egressProxy.ports`), except `networkPolicy.blockedCIDRs` and
   `clusterCIDRs`. Those ranges moved: they used to be the run pods' exception to the internet rule, and are now the proxy's
   (the same list is its `EGRESS_DENY_CIDRS`). The ranges are kept **twice** on purpose: the proxy refuses an address in them
   (and in the built-in private, loopback, link-local, shared, multicast, documentation and reserved ranges), and its policy
   would drop the connection, so a bug in one does not open the cluster.
6. **IPv6 is denied.** No policy has an IPv6 rule (`render-check.sh` asserts it on all three policies, and the kind job on the
   applied ones), so a plugin that applies the policies to IPv6 drops it, and the proxy drops every IPv6 address it resolves and
   connects over IPv4 only. A name that has only IPv6 addresses is refused (`no public address`). The proof available is the
   absence of a rule: kind is IPv4 only, so a run pod's IPv6 egress cannot be tried there.
7. **The default allow-list** (`runPods.egressProxy.allow`) holds the package registries and git hosts, exact names:

   | Ecosystem | Hosts |
   |---|---|
   | npm | `registry.npmjs.org` |
   | PyPI | `pypi.org`, `files.pythonhosted.org` |
   | crates.io | `crates.io`, `index.crates.io`, `static.crates.io` |
   | Go modules | `proxy.golang.org`, `sum.golang.org` |
   | GitHub | `github.com`, `codeload.github.com`, `objects.githubusercontent.com`, `release-assets.githubusercontent.com` |
   | Dart | `pub.dev` |

   **`storage.googleapis.com` is not on it**: pub.dev serves its package archives itself (see the facts), and that host serves
   every bucket of Google Cloud Storage, which is a place to put data as much as to fetch it. No entry is a suffix.
8. **A deployment extends the list with `runPods.egressProxy.extraAllow`** (added to `allow`, lower-cased, once each), which
   survives a change of the defaults; `allow` itself can be replaced. `example.com` is that host, `.example.com` every subdomain
   of it and not the apex. **A model gateway outside the cluster** that run pods still call (the model-key ticket,
   [#119](https://github.com/vymalo/another-adam-rs/issues/119)) goes on the list. One inside the cluster is an `extraEgress`
   rule and its host in `extraNoProxy`. A mirror or registry that speaks plain HTTP is not served by the proxy: it is an
   `extraEgress` rule too.

## What is not covered

* **Domain fronting.** A client can name an allowed host in the `CONNECT` and send another name in the TLS handshake, to the
  CDN behind it. Seeing the difference needs TLS interception, which this does not do.
* **What an allowed host accepts.** The allow-list limits *where* a run can send bytes, not *what*. An upload to an allowed
  registry or to `github.com` needs a credential, which a run pod does not hold; but a hostile prompt can bring its own (the
  attacker's npm token, a GitHub token of an account of the attacker's) and publish the repository there. This policy removes the
  open internet; it does not make an exfiltration impossible, and it is no substitute for not putting a credential or a secret in
  a run.
* **DNS.** The resolver is allowed (a name has to be resolved), and a name can carry data to a server that the attacker runs.
* **The coder's own egress, and that of agent folders.** Only run pods are changed. The coder pod reaches the model gateway,
  GitHub and the MCP servers, and its chart's `NetworkPolicy` is still ingress only.
* **A suffix entry is as wide as the domain.** `.github.io` would allow every site on it. The defaults hold none, and the chart
  refuses a bare suffix; an entry of a deployment is its own risk.
* **No authentication on the proxy.** Whoever can reach the port is a client; the proxy's policy says who that is (the pods that
  carry the run pods' labels, which the coder makes; whoever else may create pods in the namespace could label one the same way, and
  that is the RBAC's to stop).
* **Pods made before an upgrade have no proxy variables** (and no way out, under the new policy) until the idle sweep or the end
  of their run replaces them, at most `runPods.idleSecs` (900) after their last command.
* **A cluster whose plugin does not enforce `NetworkPolicy`** gets the proxy and no deny: the policies are inert.

## Facts, and how they are known

* kindnet, kind's network plugin on `kindest/node:v1.35.8`, enforces `NetworkPolicy`: the platform's `run-pods-e2e` requires the
  refusal and passed, *verified 2026-10-27* (run 40435825771). The node image is pinned in the `run-pods` job by tag and digest, as
  `operator.yml` pins it (the preinstalled `kind` runs it there, *verified 2026-10-27*: the kind jobs of run 40465377293 passed).
* The slot checks of ADR 0041 in a real run pod (a slot's directory cannot be renamed, removed or exchanged, and git works) passed
  in the `run-pods` job, *verified 2026-10-27* (run 40456829784, job 122032008830).
* pub.dev serves its archives itself: *verified 2026-10-27* by `curl`, the `archive_url` of a package in the API answer is
  `https://pub.dev/api/archives/...`, and a request for it answered 200 with no redirect (so `storage.googleapis.com` is not needed).
* A GitHub release asset redirects to `release-assets.githubusercontent.com`: *verified 2026-10-27* by `curl` on a release of
  `cli/cli` (302 to that host). `objects.githubusercontent.com` served them before and stays on the list.
* The node's access to its own pods is not blocked by a `NetworkPolicy`, so the kubelet's probes of the proxy need no rule:
  *verified 2026-10-27* in the Kubernetes documentation ("traffic to and from the node where a Pod is running is always allowed",
  and an isolated pod accepts connections "from the pod's node and those allowed by the `ingress` list").
* What the chart renders, in `deploy/coder/tests/render-check.sh`: the three policies (the run pods' has no `ipBlock`, the proxy's
  egress is 443 and the ranges), the template's six variables, `extraAllow` in the ConfigMap, the refusals, and nothing of the proxy
  with `runPods.enabled: false` (the render is the golden, byte for byte). The new checks were broken on purpose (about sixty changes to the
  templates and the values, each of the guarantees above) and each change failed the check built for it.
* In the cluster, the `run-pods` job (`deploy/coder/tests/kind-run-pods.sh`) is what proves the rest, and it is *unverified* until
  CI has run it: a direct connection from a run pod to a public address, to the API server (by its Service name, which DNS resolves,
  and by its address) and to another pod is refused after a control that the proxy's own pod reaches the public address and that a
  pod no policy selects reaches the other pod; the proxy's pod reaches no public address on 80 and no pod; `curl` and `npm view`
  through the proxy succeed; `https://example.com/` is answered 403 and the log has `decision=refuse` for it and no byte of a
  `Proxy-Authorization` header or a query sent to the proxy. The script **fails** when a policy is not enforced, unless
  `ADAM_TEST_CNI_MAY_IGNORE_POLICIES=1` says the cluster is known not to, and it retries a refusal for up to 60 s because kindnet
  programs a policy a moment after it sees a pod.
* *Unverified:* that git, cargo, pip and go read the variables (curl and npm are tried by the job; the others are documented to);
  that the owner's cluster enforces `NetworkPolicy`, and its pod and service CIDRs (`clusterCIDRs`).

## Alternatives rejected

* **Squid, Smokescreen:** see decision 2.
* **A plugin's FQDN policy:** not portable (see Context).
* **A transparent proxy or TLS interception:** a CA in every run pod and a man in the middle of every credential-less fetch, to
  answer domain fronting, which this ADR leaves open.
* **A proxy in each run pod (a sidecar):** a process in the pod that the commands share, which they could signal or replace, and
  an allow-list to roll out with every pod.
* **Opening 443 to the internet** (`0.0.0.0/0` on 443, no proxy): it is the old rule with one port off, and any host.
* **An open-egress switch for a deployment that wants it:** none. `extraEgress` and `extraAllow` are the ways to add, and each is a
  named target.

## Decisions for the owner to confirm

1. **`runPods.networkPolicy.enabled` is gone** and `false` is refused, because the switch would be the open-egress escape hatch the
   design excludes. A deployment on a plugin that has no `NetworkPolicy` support is unaffected (the policy is inert there).
2. **`release-assets.githubusercontent.com` is on the default list**, beside the design's hosts, because a release asset of
   github.com now redirects to it (a tool that downloads a binary from a release would otherwise fail). Remove it from `allow` if
   release downloads are not wanted.
3. **`runPods.egressProxy.extraNoProxy`** is new: without it an `extraEgress` rule for an in-cluster target would be useless, since
   the tools would send the request to the proxy, which refuses a name that is not on its list.
4. **Two replicas, `32Mi` and `25m` requested and `256Mi` as the limit** by default, and no anti-affinity: a node that holds both
   is a node drain away from a brief outage, which the `PodDisruptionBudget` prevents for voluntary disruptions only.
5. **The chart's version is not bumped** (0.4.0): the render with run pods off is the golden, byte for byte, and no value of a
   deployment that has run pods off changes meaning.

## Consequences

* A run's builds fetch through a proxy: a tool that ignores `HTTPS_PROXY` fails at connect, and plain `http://` fails with `403`
  and a message that says to use HTTPS. A deployment that needs a host adds it to `extraAllow` and rolls the proxy.
* The proxy is one more Deployment per release with run pods, from the coder's image, and a new failure mode: with it down, no run
  fetches anything (the run goes on and its command fails). Two replicas and the budget are the answer.
* The operator's run-pod objects (the platform's parity goldens at an adam-rs revision) follow this as a platform change after
  this one. `tools/adam-operator-parity` projects the pods of the cases (which have run pods off), so its goldens did not move.
* The ranges `blockedCIDRs` and `clusterCIDRs` mean what they say again: where nothing may connect.
* ADR 0019's OD-K6 is resolved the other way: "allow only named hosts" is the default, and its stated cost (builds that fetch from
  anywhere break) is accepted.

## Amended

* *2026-10-27*, with the change that built this: ADR 0019 (decision 7, OD-K6 and the consequence about the policy) and ADR 0041 point
  here.
