#!/usr/bin/env python3
"""Project what the adam-rs chart `deploy/coder` renders onto the shape the parity test reads.

Reads the rendered manifests (multi-document YAML) on stdin and writes one JSON document on stdout:
for every StatefulSet and Deployment its pods (the agent container, the native sidecars, volumes,
mounts, probes, resources, security context), the claims the chart makes, the ConfigMap `<svc>-mcp`
with its JSON parsed, the PodDisruptionBudgets and what the Service selects.

Only facts the operator promises to keep are projected. The chart's own conveniences (labels,
ExternalSecret, the CloudNativePG cluster, pull secrets, scheduling hints) are left out: the golden's
README lists them. Needs PyYAML (`pip install pyyaml`).
"""
import json
import sys

import yaml


def source(entry):
    """The source of an env entry, as the three kinds the operator has."""
    name = entry["name"]
    if "value" in entry:
        return {"name": name, "literal": str(entry["value"])}
    ref = entry.get("valueFrom", {})
    if "secretKeyRef" in ref:
        r = ref["secretKeyRef"]
        return {"name": name, "secret": {"name": r["name"], "key": r["key"]}}
    if "fieldRef" in ref:
        return {"name": name, "field": ref["fieldRef"]["fieldPath"]}
    raise SystemExit(f"env {name}: a value source this projection does not know: {entry}")


def probe(p):
    if p is None:
        return None
    out = {"period": p.get("periodSeconds"), "timeout": p.get("timeoutSeconds"),
           "failureThreshold": p.get("failureThreshold")}
    if "httpGet" in p:
        out["http"] = p["httpGet"]["path"]
    elif "exec" in p:
        out["exec"] = p["exec"]["command"]
    else:
        raise SystemExit(f"a probe this projection does not know: {p}")
    return out


def container(c):
    res = c.get("resources", {})
    return {
        "image": c["image"],
        "command": c.get("command", []),
        "args": c.get("args", []),
        "env": [source(e) for e in c.get("env", [])],
        "mounts": [{"name": m["name"], "path": m["mountPath"], "readOnly": bool(m.get("readOnly", False))}
                   for m in c.get("volumeMounts", [])],
        "probes": {k: probe(c.get(k + "Probe")) for k in ("startup", "liveness", "readiness")},
        "resources": {"requests": res.get("requests", {}), "limits": res.get("limits", {})},
        "port": (c.get("ports") or [{}])[0].get("containerPort"),
    }


def volume(v):
    out = {"name": v["name"]}
    if "persistentVolumeClaim" in v:
        out.update(kind="persistentVolumeClaim", claim=v["persistentVolumeClaim"]["claimName"])
    elif "secret" in v:
        s = v["secret"]
        out.update(kind="secret", secret=s["secretName"], mode=s.get("defaultMode"),
                   items=[{"key": i["key"], "path": i["path"]} for i in s.get("items", [])])
    elif "configMap" in v:
        c = v["configMap"]
        out.update(kind="configMap", configMap=c["name"], mode=c.get("defaultMode"))
    else:
        raise SystemExit(f"a volume this projection does not know: {v}")
    return out


def workload(doc):
    pod = doc["spec"]["template"]["spec"]
    sc = pod.get("securityContext", {})
    containers = pod["containers"]
    if len(containers) != 1:
        raise SystemExit("expected one agent container")
    return {
        "kind": doc["kind"],
        "replicas": doc["spec"]["replicas"],
        "terminationGracePeriodSeconds": pod.get("terminationGracePeriodSeconds"),
        "securityContext": {k: sc.get(k) for k in ("runAsUser", "runAsGroup", "fsGroup", "fsGroupChangePolicy")},
        "agent": container(containers[0]),
        "sidecars": {c["name"]: container(c) for c in pod.get("initContainers", [])
                     if c.get("restartPolicy") == "Always"},
        "volumes": [volume(v) for v in pod.get("volumes", [])],
        "claimTemplates": [
            {"name": t["metadata"]["name"], "accessModes": t["spec"]["accessModes"],
             "storage": t["spec"]["resources"]["requests"]["storage"],
             "storageClass": t["spec"].get("storageClassName")}
            for t in doc["spec"].get("volumeClaimTemplates", [])],
    }


def main():
    out = {"workloads": {}, "claims": {}, "configMaps": {}, "podDisruptionBudgets": {}, "service": None}
    for doc in yaml.safe_load_all(sys.stdin):
        if not doc:
            continue
        kind, name = doc["kind"], doc["metadata"]["name"]
        if kind in ("StatefulSet", "Deployment"):
            out["workloads"][name] = workload(doc)
        elif kind == "PersistentVolumeClaim":
            s = doc["spec"]
            out["claims"][name] = {"accessModes": s["accessModes"], "storage": s["resources"]["requests"]["storage"],
                                   "storageClass": s.get("storageClassName")}
        elif kind == "ConfigMap":
            out["configMaps"][name] = {k: json.loads(v) for k, v in doc["data"].items()}
        elif kind == "PodDisruptionBudget":
            out["podDisruptionBudgets"][name] = {"minAvailable": doc["spec"]["minAvailable"]}
        elif kind == "Service":
            out["service"] = {"name": name, "port": doc["spec"]["ports"][0]["port"],
                              "selects": doc["spec"]["selector"]["app.kubernetes.io/name"]}
    json.dump(out, sys.stdout, indent=2, sort_keys=True)
    sys.stdout.write("\n")


main()
