//! From what `adam-operator-api` embeds of Kubernetes (selectors, quantities) to the neutral types of
//! `adam-operator-ports`. The only place in this crate that names a `k8s-openapi` type.

use std::collections::BTreeMap;

use adam_operator_ports::{IpBlock, Peer, Requirement, Resources, Selector, SelectorOperator};
use k8s_openapi::api::core::v1::ResourceRequirements;
use k8s_openapi::api::networking::v1::NetworkPolicyPeer;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;

use crate::syntax::is_quantity;

fn selector(path: &str, s: &LabelSelector, problems: &mut Vec<(String, String)>) -> Selector {
    let mut expressions = Vec::new();
    for (i, e) in s.match_expressions.iter().flatten().enumerate() {
        let operator = match e.operator.as_str() {
            "In" => SelectorOperator::In,
            "NotIn" => SelectorOperator::NotIn,
            "Exists" => SelectorOperator::Exists,
            "DoesNotExist" => SelectorOperator::DoesNotExist,
            other => {
                problems.push((
                    format!("{path}.matchExpressions[{i}].operator"),
                    format!("{other:?} is not In, NotIn, Exists or DoesNotExist"),
                ));
                continue;
            }
        };
        let values = e.values.clone().unwrap_or_default();
        let wants_values = matches!(operator, SelectorOperator::In | SelectorOperator::NotIn);
        if wants_values == values.is_empty() {
            problems.push((
                format!("{path}.matchExpressions[{i}].values"),
                if wants_values {
                    "In and NotIn need at least one value".to_owned()
                } else {
                    "Exists and DoesNotExist take no values".to_owned()
                },
            ));
        }
        if e.key.is_empty() {
            problems.push((
                format!("{path}.matchExpressions[{i}].key"),
                "the key is empty".to_owned(),
            ));
        }
        expressions.push(Requirement {
            key: e.key.clone(),
            operator,
            values,
        });
    }
    Selector {
        match_labels: s.match_labels.clone().unwrap_or_default(),
        match_expressions: expressions,
    }
}

/// `access.allowFrom` as neutral peers. Problems are `(field, message)` pairs: a peer sets at least
/// one of its three fields, an address range is not combined with a selector (Kubernetes refuses
/// it), and a selector's operators and values agree.
pub fn peers(peers: &[NetworkPolicyPeer]) -> (Vec<Peer>, Vec<(String, String)>) {
    let mut problems = Vec::new();
    let mut out = Vec::new();
    for (i, p) in peers.iter().enumerate() {
        let path = format!("spec.access.allowFrom[{i}]");
        let set = [
            p.ip_block.is_some(),
            p.namespace_selector.is_some(),
            p.pod_selector.is_some(),
        ]
        .into_iter()
        .filter(|s| *s)
        .count();
        if set == 0 {
            problems.push((
                path.clone(),
                "a peer needs a namespaceSelector, a podSelector or an ipBlock".to_owned(),
            ));
        }
        if p.ip_block.is_some() && set > 1 {
            problems.push((
                path.clone(),
                "an ipBlock cannot be combined with a selector in one peer".to_owned(),
            ));
        }
        if let Some(b) = &p.ip_block
            && b.cidr.trim().is_empty()
        {
            problems.push((
                format!("{path}.ipBlock.cidr"),
                "the cidr is empty".to_owned(),
            ));
        }
        out.push(Peer {
            namespaces: p
                .namespace_selector
                .as_ref()
                .map(|s| selector(&format!("{path}.namespaceSelector"), s, &mut problems)),
            pods: p
                .pod_selector
                .as_ref()
                .map(|s| selector(&format!("{path}.podSelector"), s, &mut problems)),
            cidr: p.ip_block.as_ref().map(|b| IpBlock {
                cidr: b.cidr.clone(),
                except: b.except.clone().unwrap_or_default(),
            }),
        });
    }
    (out, problems)
}

/// `environment.resources` as quantity strings. Claims are not carried (v0 has no device
/// requests). Problems are `(field, message)` pairs for a quantity that is not one.
pub fn resources(r: Option<&ResourceRequirements>) -> (Resources, Vec<(String, String)>) {
    let mut problems = Vec::new();
    let mut take = |kind: &str,
                    m: Option<
        &BTreeMap<String, k8s_openapi::apimachinery::pkg::api::resource::Quantity>,
    >| {
        let mut out = BTreeMap::new();
        for (k, q) in m.into_iter().flatten() {
            if !is_quantity(&q.0) {
                problems.push((
                    format!("spec.environment.resources.{kind}.{k}"),
                    format!("{:?} is not a quantity", q.0),
                ));
            }
            out.insert(k.clone(), q.0.clone());
        }
        out
    };
    let requests = take("requests", r.and_then(|r| r.requests.as_ref()));
    let limits = take("limits", r.and_then(|r| r.limits.as_ref()));
    (Resources { requests, limits }, problems)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::resources;
    use k8s_openapi::api::core::v1::ResourceRequirements;
    use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
    use std::collections::BTreeMap;

    fn reqs(pairs: &[(&str, &str)]) -> Option<BTreeMap<String, Quantity>> {
        Some(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), Quantity((*v).to_owned())))
                .collect(),
        )
    }

    #[test]
    fn a_zero_request_is_a_quantity() {
        // Kubernetes takes `cpu: "0"` (count on the limit alone): it is not refused.
        let r = ResourceRequirements {
            requests: reqs(&[("cpu", "0"), ("memory", "0m")]),
            limits: reqs(&[("memory", "2Gi")]),
            ..Default::default()
        };
        let (out, problems) = resources(Some(&r));
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(out.requests.get("cpu").map(String::as_str), Some("0"));
    }

    #[test]
    fn a_resource_that_is_not_a_quantity_is_a_problem() {
        let r = ResourceRequirements {
            limits: reqs(&[("memory", "2 Gi")]),
            ..Default::default()
        };
        let (_, problems) = resources(Some(&r));
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(problems[0].0, "spec.environment.resources.limits.memory");
    }
}
