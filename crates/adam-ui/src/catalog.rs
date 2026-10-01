//! The screen's catalog: the components the person's screen can draw, as the web defines them
//! (`docs/api/ui-catalog-v1.md` of `vymalo/another-agentic-system`).
//!
//! The document is an A2UI inline catalog, `{catalogId, components: {<Name>: <JSON Schema>}}`. Its
//! **digest** is `sha256:` and the lowercase hex of SHA-256 over its canonical JSON: object keys
//! sorted, no whitespace, strings escaped as `serde_json` and `JSON.stringify` do, integers in
//! decimal (RFC 8785, restricted to what a catalog may hold: ASCII keys, whole numbers). A catalog
//! read here is **checked against the digest it claims**: a document that is not what the sender
//! says it is, is refused and never drawn with.
//!
//! An A2A server hands the numbers of a message's metadata over as doubles, so an inline catalog
//! reads `maxLength: 256.0`. Every whole number is written back as an integer before anything else
//! (RFC 8785 writes `256.0` as `256`), and the digest computed is then the one the sender computed.

use std::fmt::Write as _;

use adam_a2a_runtime::integral_numbers;
use jsonschema::Validator;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// The most bytes (serialised JSON) a catalog may hold: the contract's limit.
pub const MAX_CATALOG_BYTES: usize = 64 * 1024;

/// The most components a catalog may hold: the contract's limit.
pub const MAX_COMPONENTS: usize = 64;

/// How deep a catalog may nest.
const MAX_DEPTH: usize = 32;

/// The largest whole number a catalog may hold (2^53 - 1, exact in a double).
const MAX_SAFE_INTEGER: i64 = (1 << 53) - 1;

/// The most validation problems one message about an instance lists.
const MAX_PROBLEMS: usize = 3;

/// Why a catalog was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CatalogError {
    /// The value is not `{catalogId, components}` or is not the catalog it claims to be.
    #[error("not a UI catalog: {0}")]
    Shape(String),
    /// Larger than [`MAX_CATALOG_BYTES`].
    #[error("the catalog is larger than {MAX_CATALOG_BYTES} bytes")]
    TooLarge,
    /// A key that is not ASCII, a number that is not a whole number in range, or nesting deeper
    /// than the limit: what the canonical form does not cover.
    #[error("the catalog cannot be put in canonical form: {0}")]
    NotCanonical(String),
    /// The digest the sender claims is not the digest of the document.
    #[error("the digest of the catalog is {computed}, not the {claimed} it was announced with")]
    DigestMismatch {
        /// What the sender said.
        claimed: String,
        /// What the document hashes to.
        computed: String,
    },
    /// A component's schema is not a JSON Schema (2020-12) that compiles.
    #[error("the schema of component `{component}` does not compile: {message}")]
    Schema {
        /// The component.
        component: String,
        /// Why.
        message: String,
    },
}

/// What a sender says a catalog is: announced beside it, checked against it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claimed {
    /// `catalogId`.
    pub catalog_id: String,
    /// The catalog's version, from 1.
    pub version: u32,
    /// `sha256:` and 64 hex digits.
    pub digest: String,
}

/// One component of a catalog: its name, what it is for, and its compiled schema.
pub struct Component {
    name: String,
    description: String,
    schema: Value,
    validator: Validator,
}

impl Component {
    /// The component's name (the `component` of an instance).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// What the component is for, as the catalog says (the schema's `description`).
    pub fn description(&self) -> &str {
        &self.description
    }

    /// The JSON Schema of an instance of the component.
    pub fn schema(&self) -> &Value {
        &self.schema
    }
}

impl std::fmt::Debug for Component {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Component")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// A catalog that was read, checked and compiled: its components can validate an instance.
#[derive(Debug)]
pub struct Catalog {
    catalog_id: String,
    version: u32,
    digest: String,
    document: Value,
    components: Vec<Component>,
}

impl Catalog {
    /// Read `document` as the catalog `claimed` says it is.
    ///
    /// The numbers are made whole (see the module docs), the document must be exactly
    /// `{catalogId, components}` with the `catalogId` claimed, within the contract's size and count
    /// limits, its digest must be the claimed one, and every component's schema must compile.
    ///
    /// # Errors
    ///
    /// [`CatalogError`], the first problem found.
    pub fn from_document(mut document: Value, claimed: &Claimed) -> Result<Self, CatalogError> {
        integral_numbers(&mut document);
        let Value::Object(members) = &document else {
            return Err(CatalogError::Shape("it is not an object".into()));
        };
        if let Some(extra) = members
            .keys()
            .find(|k| !matches!(k.as_str(), "catalogId" | "components"))
        {
            return Err(CatalogError::Shape(format!(
                "it has a member `{extra}`; only catalogId and components are accepted"
            )));
        }
        if members.get("catalogId").and_then(Value::as_str) != Some(claimed.catalog_id.as_str()) {
            return Err(CatalogError::Shape(
                "its catalogId is not the one it was announced with".into(),
            ));
        }
        let Some(Value::Object(components)) = members.get("components") else {
            return Err(CatalogError::Shape("components must be an object".into()));
        };
        if components.is_empty() || components.len() > MAX_COMPONENTS {
            return Err(CatalogError::Shape(format!(
                "it must have 1 to {MAX_COMPONENTS} components, not {}",
                components.len()
            )));
        }
        let canonical = canonical_json(&document)?;
        if canonical.len() > MAX_CATALOG_BYTES {
            return Err(CatalogError::TooLarge);
        }
        let computed = digest_of_canonical(&canonical);
        if computed != claimed.digest {
            return Err(CatalogError::DigestMismatch {
                claimed: claimed.digest.clone(),
                computed,
            });
        }
        let mut compiled = Vec::with_capacity(components.len());
        for (name, schema) in components {
            let validator =
                jsonschema::draft202012::new(schema).map_err(|e| CatalogError::Schema {
                    component: name.clone(),
                    message: e.to_string(),
                })?;
            compiled.push(Component {
                name: name.clone(),
                description: schema
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                schema: schema.clone(),
                validator,
            });
        }
        // By name, whatever order the map keeps its members in (a dependency can change it for the
        // whole build): what the model reads is the same on every build.
        compiled.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Self {
            catalog_id: claimed.catalog_id.clone(),
            version: claimed.version,
            digest: claimed.digest.clone(),
            document,
            components: compiled,
        })
    }

    /// The `catalogId` surfaces under this catalog name in `createSurface`.
    pub fn catalog_id(&self) -> &str {
        &self.catalog_id
    }

    /// The version the sender announced, from 1.
    pub fn version(&self) -> u32 {
        self.version
    }

    /// The digest: `sha256:` and 64 hex digits.
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// The document, with whole numbers as integers.
    pub fn document(&self) -> &Value {
        &self.document
    }

    /// The components, sorted by name.
    pub fn components(&self) -> &[Component] {
        &self.components
    }

    /// The component called `name`.
    pub fn component(&self, name: &str) -> Option<&Component> {
        self.components.iter().find(|c| c.name == name)
    }

    /// The components' names, comma-separated, for a message to the model.
    pub fn names(&self) -> String {
        self.components
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Check an instance `{id, component, ...}` against its component's schema.
    ///
    /// # Errors
    ///
    /// What is wrong, for the model that wrote it: the component is not in the catalog (the ones
    /// that are are listed), or the first few violations of the schema, each with its place in the
    /// instance.
    pub fn validate(&self, instance: &Value) -> Result<(), String> {
        let name = instance
            .get("component")
            .and_then(Value::as_str)
            .ok_or_else(|| "it has no `component`".to_owned())?;
        let component = self.component(name).ok_or_else(|| {
            format!(
                "`{name}` is not a component of this screen; the components are: {}",
                self.names()
            )
        })?;
        let problems: Vec<String> = component
            .validator
            .iter_errors(instance)
            .take(MAX_PROBLEMS + 1)
            .map(|e| {
                let at = e.instance_path().to_string();
                if at.is_empty() {
                    e.to_string()
                } else {
                    format!("{e} (at {at})")
                }
            })
            .collect();
        if problems.is_empty() {
            return Ok(());
        }
        let more = problems.len() > MAX_PROBLEMS;
        let mut message = problems
            .into_iter()
            .take(MAX_PROBLEMS)
            .collect::<Vec<_>>()
            .join("; ");
        if more {
            message.push_str("; and more");
        }
        Err(message)
    }

    /// The catalog as the model reads it: for each component its name, what it is for and the schema
    /// of an instance.
    pub fn describe(&self) -> Value {
        let components: Vec<Value> = self
            .components
            .iter()
            .map(|c| {
                let mut entry = Map::new();
                entry.insert("name".into(), Value::String(c.name.clone()));
                entry.insert("description".into(), Value::String(c.description.clone()));
                entry.insert("schema".into(), c.schema.clone());
                Value::Object(entry)
            })
            .collect();
        serde_json::json!({
            "catalogId": self.catalog_id,
            "version": self.version,
            "digest": self.digest,
            "components": components,
        })
    }
}

/// The canonical JSON of `value`: the bytes the digest is taken over.
///
/// Object keys are sorted (by code point, which for the ASCII keys that are allowed is the byte
/// order, and also the UTF-16 order JavaScript sorts by), there is no whitespace, strings are
/// escaped as `serde_json` and `JSON.stringify` do, and integers are in decimal. It is written
/// out, not left to `serde_json`'s map order, which a dependency can change for the whole build
/// (`preserve_order`). A number written `256.0` counts as `256`.
///
/// # Errors
///
/// [`CatalogError::NotCanonical`]: a key that is not ASCII, a number that is not a whole number
/// within +-(2^53 - 1), or nesting deeper than 32 levels.
pub fn canonical_json(value: &Value) -> Result<String, CatalogError> {
    let mut out = String::new();
    write_canonical(value, 0, &mut out)?;
    Ok(out)
}

fn write_canonical(value: &Value, depth: usize, out: &mut String) -> Result<(), CatalogError> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => {
            let integer = n
                .as_i64()
                .or_else(|| {
                    n.as_f64()
                        .filter(|f| f.fract() == 0.0 && f.abs() <= MAX_SAFE_INTEGER as f64)
                        .map(|f| f as i64)
                })
                .filter(|i| i.unsigned_abs() <= MAX_SAFE_INTEGER.unsigned_abs())
                .ok_or_else(|| {
                    CatalogError::NotCanonical(format!("{n} is not a whole number in range"))
                })?;
            // Writing to a String does not fail.
            let _ = write!(out, "{integer}");
        }
        Value::String(s) => out.push_str(&quoted(s)?),
        Value::Array(items) => {
            nested(depth)?;
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, depth + 1, out)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            nested(depth)?;
            let mut keys: Vec<&String> = map.keys().collect();
            if let Some(key) = keys.iter().find(|k| !k.is_ascii()) {
                return Err(CatalogError::NotCanonical(format!(
                    "the key `{key}` is not ASCII"
                )));
            }
            keys.sort_unstable();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&quoted(key)?);
                out.push(':');
                if let Some(inner) = map.get(key) {
                    write_canonical(inner, depth + 1, out)?;
                }
            }
            out.push('}');
        }
    }
    Ok(())
}

fn nested(depth: usize) -> Result<(), CatalogError> {
    if depth >= MAX_DEPTH {
        Err(CatalogError::NotCanonical(format!(
            "it nests deeper than {MAX_DEPTH} levels"
        )))
    } else {
        Ok(())
    }
}

fn quoted(text: &str) -> Result<String, CatalogError> {
    serde_json::to_string(text).map_err(|e| CatalogError::NotCanonical(e.to_string()))
}

fn digest_of_canonical(canonical: &str) -> String {
    let hash = Sha256::digest(canonical.as_bytes());
    let mut out = String::from("sha256:");
    for byte in hash {
        // Writing to a String does not fail.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The digest of a catalog document: `sha256:` and the lowercase hex of SHA-256 over its
/// [`canonical_json`].
///
/// # Errors
///
/// What [`canonical_json`] refuses.
pub fn catalog_digest(document: &Value) -> Result<String, CatalogError> {
    canonical_json(document).map(|canonical| digest_of_canonical(&canonical))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// The known-answer vector of the contract (computed 2026-10-01 with Python's `json.dumps(
    /// sort_keys=True, separators=(',', ':'), ensure_ascii=False)` and SHA-256): the web, the
    /// orchestrator and adam-rs all pin it.
    fn vector() -> Value {
        json!({
            "catalogId": "https://agents.vymalo.com/a2ui/catalogs/test",
            "components": {"Note": {
                "type": "object",
                "properties": {"component": {"const": "Note"}, "text": {"type": "string", "maxLength": 10}},
                "required": ["component", "text"]}}
        })
    }

    const VECTOR_CANONICAL: &str = r#"{"catalogId":"https://agents.vymalo.com/a2ui/catalogs/test","components":{"Note":{"properties":{"component":{"const":"Note"},"text":{"maxLength":10,"type":"string"}},"required":["component","text"],"type":"object"}}}"#;
    const VECTOR_DIGEST: &str =
        "sha256:a237e931c3a02fc72b214561e3a52d33eaf0e29c9306bbe0ef7b3da238507293";

    fn claimed(digest: &str) -> Claimed {
        Claimed {
            catalog_id: "https://agents.vymalo.com/a2ui/catalogs/test".into(),
            version: 1,
            digest: digest.into(),
        }
    }

    #[test]
    fn the_known_answer_vector_gives_the_contracts_canonical_form_and_digest() {
        assert_eq!(canonical_json(&vector()).unwrap(), VECTOR_CANONICAL);
        assert_eq!(catalog_digest(&vector()).unwrap(), VECTOR_DIGEST);
    }

    #[test]
    fn whole_doubles_hash_as_the_integers_they_are() {
        // As an A2A server hands the metadata over: every number a double.
        let mut doubles = vector();
        doubles["components"]["Note"]["properties"]["text"]["maxLength"] = json!(10.0);
        assert!(doubles["components"]["Note"]["properties"]["text"]["maxLength"].is_f64());
        assert_eq!(catalog_digest(&doubles).unwrap(), VECTOR_DIGEST);
        let catalog = Catalog::from_document(doubles, &claimed(VECTOR_DIGEST)).unwrap();
        assert!(
            catalog.document()["components"]["Note"]["properties"]["text"]["maxLength"].is_i64()
        );
    }

    #[test]
    fn what_the_canonical_form_does_not_cover_is_refused_not_guessed() {
        for (bad, why) in [
            (json!({"a": 1.5}), "1.5"),
            (json!({"a": 9007199254740992.0}), "9007199254740992"),
            (json!({"é": 1}), "not ASCII"),
        ] {
            let error = canonical_json(&bad).unwrap_err();
            assert!(
                matches!(&error, CatalogError::NotCanonical(m) if m.contains(why)),
                "{bad}: {error}"
            );
        }
        let mut deep = json!(1);
        for _ in 0..40 {
            deep = json!([deep]);
        }
        assert!(matches!(
            canonical_json(&deep),
            Err(CatalogError::NotCanonical(m)) if m.contains("deeper")
        ));
        // Keys are sorted, whatever order the map keeps them in.
        assert_eq!(
            canonical_json(&json!({"b": [true, null], "a": "x\n\"é\""})).unwrap(),
            "{\"a\":\"x\\n\\\"é\\\"\",\"b\":[true,null]}"
        );
    }

    #[test]
    fn a_catalog_is_checked_against_the_digest_it_claims() {
        let ok = Catalog::from_document(vector(), &claimed(VECTOR_DIGEST)).unwrap();
        assert_eq!(ok.digest(), VECTOR_DIGEST);
        assert_eq!(ok.version(), 1);
        assert_eq!(ok.names(), "Note");
        assert_eq!(ok.components().len(), 1);

        let other = format!("sha256:{}", "0".repeat(64));
        let error = Catalog::from_document(vector(), &claimed(&other)).unwrap_err();
        assert!(
            matches!(&error, CatalogError::DigestMismatch { claimed, computed }
                if claimed == &other && computed == VECTOR_DIGEST),
            "{error}"
        );
    }

    #[test]
    fn a_document_that_is_not_the_catalog_it_says_is_refused() {
        let mut other_id = vector();
        other_id["catalogId"] = json!("https://example.com/other");
        assert!(matches!(
            Catalog::from_document(other_id, &claimed(VECTOR_DIGEST)),
            Err(CatalogError::Shape(_))
        ));
        let mut extra = vector();
        extra["theme"] = json!({});
        assert!(matches!(
            Catalog::from_document(extra, &claimed(VECTOR_DIGEST)),
            Err(CatalogError::Shape(m)) if m.contains("theme")
        ));
        for no_components in [
            json!({"catalogId": "https://agents.vymalo.com/a2ui/catalogs/test"}),
            json!({"catalogId": "https://agents.vymalo.com/a2ui/catalogs/test", "components": {}}),
            json!({"catalogId": "https://agents.vymalo.com/a2ui/catalogs/test", "components": []}),
        ] {
            assert!(matches!(
                Catalog::from_document(no_components, &claimed(VECTOR_DIGEST)),
                Err(CatalogError::Shape(_))
            ));
        }
        assert!(matches!(
            Catalog::from_document(json!([1]), &claimed(VECTOR_DIGEST)),
            Err(CatalogError::Shape(_))
        ));
    }

    #[test]
    fn a_schema_that_does_not_compile_refuses_the_catalog_naming_the_component() {
        let broken = json!({
            "catalogId": "https://agents.vymalo.com/a2ui/catalogs/test",
            "components": {"Bad": {"type": 5}}
        });
        let digest = catalog_digest(&broken).unwrap();
        let error = Catalog::from_document(broken, &claimed(&digest)).unwrap_err();
        assert!(
            matches!(&error, CatalogError::Schema { component, .. } if component == "Bad"),
            "{error}"
        );
    }

    #[test]
    fn an_oversized_catalog_is_refused() {
        let big = json!({
            "catalogId": "https://agents.vymalo.com/a2ui/catalogs/test",
            "components": {"Note": {"description": "x".repeat(MAX_CATALOG_BYTES)}}
        });
        let digest = catalog_digest(&big).unwrap();
        assert_eq!(
            Catalog::from_document(big, &claimed(&digest)).unwrap_err(),
            CatalogError::TooLarge
        );
    }

    #[test]
    fn an_instance_is_checked_against_its_component_and_the_message_is_for_the_model() {
        let catalog = Catalog::from_document(vector(), &claimed(VECTOR_DIGEST)).unwrap();
        assert!(
            catalog
                .validate(&json!({"component": "Note", "text": "hi"}))
                .is_ok()
        );
        let long = catalog
            .validate(&json!({"component": "Note", "text": "this is too long"}))
            .unwrap_err();
        assert!(long.contains("(at /text)"), "{long}");
        let missing = catalog.validate(&json!({"component": "Note"})).unwrap_err();
        assert!(missing.contains("text"), "{missing}");
        let unknown = catalog.validate(&json!({"component": "Card"})).unwrap_err();
        assert_eq!(
            unknown,
            "`Card` is not a component of this screen; the components are: Note"
        );
        assert_eq!(
            catalog.validate(&json!({"text": "x"})).unwrap_err(),
            "it has no `component`"
        );
        // At most three problems are listed.
        let many = Catalog::from_document(
            {
                let doc = json!({"catalogId": "https://agents.vymalo.com/a2ui/catalogs/test",
                    "components": {"M": {"type": "object", "required": ["a", "b", "c", "d", "e"]}}});
                doc
            },
            &claimed(
                &catalog_digest(&json!({"catalogId": "https://agents.vymalo.com/a2ui/catalogs/test",
                    "components": {"M": {"type": "object", "required": ["a", "b", "c", "d", "e"]}}}))
                .unwrap(),
            ),
        )
        .unwrap();
        let message = many.validate(&json!({"component": "M"})).unwrap_err();
        assert!(message.ends_with("; and more"), "{message}");
        assert_eq!(message.matches("; ").count(), 3, "{message}");
    }

    #[test]
    fn describe_lists_each_component_with_its_description_and_schema() {
        let described = Catalog::from_document(vector(), &claimed(VECTOR_DIGEST))
            .unwrap()
            .describe();
        assert_eq!(described["version"], 1);
        assert_eq!(described["components"][0]["name"], "Note");
        assert_eq!(described["components"][0]["description"], "");
        assert_eq!(described["components"][0]["schema"]["type"], "object");
    }
}
