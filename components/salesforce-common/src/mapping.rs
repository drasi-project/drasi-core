use anyhow::{bail, ensure, Context, Result};
use apache_avro::{types::Value as Avro, Schema};
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use crate::projection::{
    graph_element_id, object, unbox, Field, Kind, ObjectPlan, Properties, Record,
};

pub const SOURCE: &str = "salesforce";
impl Record {
    pub fn change(&self, plan: &ObjectPlan) -> SourceChange {
        node_change(plan, &self.id, &self.properties, self.timestamp, true)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    Create,
    Update,
    Delete,
    Undelete,
    Reconcile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalidation {
    pub operation: Operation,
    pub ids: Vec<String>,
    pub timestamp: u64,
}

pub struct Change {
    pub operation: Operation,
    pub ids: Vec<String>,
    pub properties: Properties,
    pub diffs: BTreeMap<String, String>,
    pub timestamp: u64,
}

pub struct Decoder {
    avro: Schema,
    names: Vec<String>,
    compounds: HashMap<usize, Vec<String>>,
    plan: ObjectPlan,
}

impl Decoder {
    pub fn new(schema: &str, plan: &ObjectPlan) -> Result<Self> {
        let decoder = Self::for_invalidations(schema, plan)?;
        for selected in &plan.fields {
            ensure!(
                decoder.names.contains(&selected.name),
                "Selected field {} is absent from native CDC schema",
                selected.name
            );
        }
        Ok(decoder)
    }

    pub fn for_invalidations(schema: &str, plan: &ObjectPlan) -> Result<Self> {
        let json: Value = serde_json::from_str(schema)?;
        let names = json["fields"]
            .as_array()
            .context("Expected Avro record schema")?
            .iter()
            .map(|field| {
                field["name"]
                    .as_str()
                    .map(str::to_owned)
                    .context("Avro field has no name")
            })
            .collect::<Result<Vec<_>>>()?;
        let mut records = HashMap::new();
        collect_records(&json, &mut records);
        let compounds = json["fields"]
            .as_array()
            .context("Missing schema fields")?
            .iter()
            .enumerate()
            .filter_map(|(index, field)| {
                record_names(&field["type"], &records).map(|names| (index, names))
            })
            .collect();
        Ok(Self {
            avro: Schema::parse_str(schema)?,
            names,
            compounds,
            plan: plan.clone(),
        })
    }

    pub fn decode(&self, payload: &[u8]) -> Result<Change> {
        let (record, operation, ids, timestamp) = self.decode_header(payload)?;
        ensure!(
            operation != Operation::Reconcile,
            "CDC gap requires a complete REST reconciliation"
        );
        let mut properties = Properties::new();
        let mut text_diffs = BTreeMap::new();
        if operation != Operation::Delete {
            let header = field(&record, "ChangeEventHeader")?;
            let Avro::Record(header) = header else {
                bail!("Missing CDC header record");
            };
            let changed = self.bitmap(field(header, "changedFields")?)?;
            let nulled = self.bitmap(field(header, "nulledFields")?)?;
            let diffs = self.bitmap(field(header, "diffFields")?)?;
            for selected in &self.plan.fields {
                let path = vec![selected.name.clone()];
                if diffs.contains(&path) {
                    ensure!(
                        matches!(selected.kind, Kind::Text),
                        "Diff applied to a non-text field"
                    );
                    text_diffs.insert(
                        selected.name.clone(),
                        string(field(&record, &selected.name)?)?.into(),
                    );
                    continue;
                }
                if let Some(value) = project(
                    selected,
                    field(&record, &selected.name)?,
                    &path,
                    operation != Operation::Update,
                    &changed,
                    &nulled,
                )? {
                    properties.insert(selected.name.clone(), value);
                }
            }
        }
        Ok(Change {
            operation,
            ids,
            properties,
            diffs: text_diffs,
            timestamp,
        })
    }

    pub fn decode_invalidation(&self, payload: &[u8]) -> Result<Invalidation> {
        let (_, operation, ids, timestamp) = self.decode_header(payload)?;
        Ok(Invalidation {
            operation,
            ids,
            timestamp,
        })
    }

    fn decode_header(
        &self,
        payload: &[u8],
    ) -> Result<(Vec<(String, Avro)>, Operation, Vec<String>, u64)> {
        let mut remaining = payload;
        let value = apache_avro::from_avro_datum(&self.avro, &mut remaining, None)?;
        ensure!(remaining.is_empty(), "Trailing bytes in Avro event");
        let Avro::Record(record) = value else {
            bail!("Expected CDC record");
        };
        let header = field(&record, "ChangeEventHeader")?;
        let Avro::Record(header) = header else {
            bail!("Missing CDC header record");
        };
        ensure!(
            string(field(header, "entityName")?)? == self.plan.name,
            "Unexpected CDC entity"
        );
        let Avro::Enum(_, change_type) = field(header, "changeType")? else {
            bail!("Expected CDC changeType enum");
        };
        let operation = match change_type.as_str() {
            "CREATE" => Operation::Create,
            "UPDATE" => Operation::Update,
            "DELETE" => Operation::Delete,
            "UNDELETE" => Operation::Undelete,
            "GAP_CREATE" | "GAP_UPDATE" | "GAP_DELETE" | "GAP_UNDELETE" | "GAP_OVERFLOW"
            | "SNAPSHOT" => Operation::Reconcile,
            other => bail!("Unsupported CDC operation {other}"),
        };
        let ids = if operation == Operation::Reconcile {
            Vec::new()
        } else {
            let ids = strings(field(header, "recordIds")?)?;
            ensure!(!ids.is_empty(), "CDC event contains no record IDs");
            ids.into_iter()
                .map(|id| self.plan.canonical_record_id(&id))
                .collect::<Result<Vec<_>>>()?
        };
        let timestamp = match field(header, "commitTimestamp")? {
            Avro::Long(value) => u64::try_from(*value)?,
            _ => bail!("Invalid CDC commitTimestamp"),
        };
        Ok((record, operation, ids, timestamp))
    }

    fn bitmap(&self, value: &Avro) -> Result<HashSet<Vec<String>>> {
        let mut names = HashSet::new();
        for bitmap in strings(value)? {
            let (prefix, bitmap, fields) = if let Some((parent, bitmap)) = bitmap.split_once('-') {
                let index: usize = parent.parse().context("Invalid compound bitmap index")?;
                let parent = self
                    .names
                    .get(index)
                    .context("Compound bitmap index exceeds schema")?;
                let fields = self
                    .compounds
                    .get(&index)
                    .context("Bitmap refers to a non-compound field")?;
                (vec![parent.clone()], bitmap, fields)
            } else {
                (Vec::new(), bitmap.as_str(), &self.names)
            };
            let hex = bitmap
                .strip_prefix("0x")
                .context("Invalid field bitmap prefix")?;
            ensure!(!hex.is_empty(), "Empty field bitmap");
            for (nibble_index, character) in hex.chars().rev().enumerate() {
                let nibble = character
                    .to_digit(16)
                    .context("Invalid hexadecimal bitmap")?;
                for bit in 0..4 {
                    if nibble & (1 << bit) != 0 {
                        let index = nibble_index * 4 + bit;
                        let name = fields.get(index).context("Bitmap index exceeds schema")?;
                        let mut path = prefix.clone();
                        path.push(name.clone());
                        names.insert(path);
                    }
                }
            }
        }
        Ok(names)
    }
}

fn project(
    selected: &Field,
    value: &Avro,
    path: &[String],
    full: bool,
    changed: &HashSet<Vec<String>>,
    nulled: &HashSet<Vec<String>>,
) -> Result<Option<ElementValue>> {
    let cleared = nulled.contains(path);
    if let Kind::Compound(parts) = &selected.kind {
        if cleared || (full && matches!(unbox(value), Avro::Null)) {
            return Ok(Some(selected.empty_compound()?));
        }
        let record = match unbox(value) {
            Avro::Record(record) => Some(record),
            Avro::Null => None,
            _ => bail!("Invalid compound field {}", selected.name),
        };
        let mut values = Vec::new();
        for part in parts {
            let mut child_path = path.to_vec();
            child_path.push(part.name.clone());
            let child = record
                .map(|record| field(record, &part.name))
                .transpose()?
                .unwrap_or(&Avro::Null);
            if let Some(value) = project(part, child, &child_path, full, changed, nulled)? {
                values.push((part.name.clone(), value));
            }
        }
        return Ok((full || !values.is_empty()).then(|| object(values)));
    }
    if cleared {
        return Ok(Some(selected.null()?));
    }
    if full || changed.contains(path) {
        ensure!(
            full || !matches!(unbox(value), Avro::Null),
            "Changed field {} is null without a nulledFields entry",
            selected.name
        );
        return Ok(Some(selected.decode_avro(value)?));
    }
    Ok(None)
}

fn collect_records(value: &Value, records: &mut HashMap<String, Vec<String>>) {
    match value {
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("record") {
                if let (Some(name), Some(fields)) = (
                    map.get("name").and_then(Value::as_str),
                    map.get("fields").and_then(Value::as_array),
                ) {
                    records.insert(
                        name.into(),
                        fields
                            .iter()
                            .filter_map(|field| field["name"].as_str().map(str::to_owned))
                            .collect(),
                    );
                }
            }
            for value in map.values() {
                collect_records(value, records);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_records(value, records);
            }
        }
        _ => {}
    }
}

fn record_names(value: &Value, records: &HashMap<String, Vec<String>>) -> Option<Vec<String>> {
    match value {
        Value::String(name) => records.get(name).cloned(),
        Value::Array(types) => types.iter().find_map(|value| record_names(value, records)),
        Value::Object(map) if map.get("type").and_then(Value::as_str) == Some("record") => map
            .get("name")
            .and_then(Value::as_str)
            .and_then(|name| records.get(name).cloned()),
        _ => None,
    }
}

pub fn apply_text_diff(previous: &str, patch: &str) -> Result<String> {
    let mut lines = patch.splitn(3, '\n');
    ensure!(
        lines.next().is_some_and(|line| line.starts_with("---")),
        "Invalid diff old-file header"
    );
    let hash = lines
        .next()
        .and_then(|line| line.strip_prefix("+++ "))
        .context("Missing diff hash")?;
    ensure!(
        hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
        "Invalid diff SHA-256"
    );
    let hunks = lines.next().context("Missing diff hunks")?;
    let mut normalized = format!("--- original\n+++ modified\n{hunks}");
    // Salesforce can omit the patch terminator; it is not a file-content newline.
    if !normalized.ends_with('\n') {
        normalized.push('\n');
    }
    let patch = diffy::Patch::from_str(&normalized).context("Invalid Salesforce unified diff")?;
    let value = diffy::apply(previous, &patch).context("Cannot apply Salesforce text diff")?;
    ensure!(
        format!("{:x}", Sha256::digest(value.as_bytes())).eq_ignore_ascii_case(hash),
        "Reconstructed text diff hash mismatch"
    );
    Ok(value)
}

fn field<'a>(record: &'a [(String, Avro)], name: &str) -> Result<&'a Avro> {
    let value = &record
        .iter()
        .find(|(key, _)| key == name)
        .with_context(|| format!("Missing Avro field {name}"))?
        .1;
    Ok(match value {
        Avro::Union(_, value) => value,
        other => other,
    })
}

fn string(value: &Avro) -> Result<&str> {
    match value {
        Avro::String(value) => Ok(value),
        _ => bail!("Expected Avro string"),
    }
}

fn strings(value: &Avro) -> Result<Vec<String>> {
    match value {
        Avro::Array(items) => items.iter().map(|v| string(v).map(str::to_owned)).collect(),
        _ => bail!("Expected Avro string array"),
    }
}

fn metadata(plan: &ObjectPlan, id: &str, timestamp: u64) -> ElementMetadata {
    ElementMetadata {
        reference: ElementReference::new(SOURCE, &format!("{}:{id}", plan.name)),
        labels: vec![Arc::from(plan.name.as_str())].into(),
        effective_from: timestamp,
    }
}

pub fn node_change(
    plan: &ObjectPlan,
    id: &str,
    values: &Properties,
    timestamp: u64,
    insert: bool,
) -> SourceChange {
    let mut properties = ElementPropertyMap::new();
    properties.insert("Id", ElementValue::String(Arc::from(id)));
    for (name, value) in values {
        properties.insert(name.as_str(), value.clone());
    }
    let element = Element::Node {
        metadata: metadata(plan, id, timestamp),
        properties,
    };
    if insert {
        SourceChange::Insert { element }
    } else {
        SourceChange::Update { element }
    }
}

pub fn node_change_with_org(
    source_id: &str,
    org_id: &str,
    plan: &ObjectPlan,
    id: &str,
    values: &Properties,
    timestamp: u64,
    insert: bool,
) -> Result<SourceChange> {
    let mut properties = ElementPropertyMap::new();
    let canonical = plan.canonical_record_id(id)?;
    let element_id = graph_element_id(org_id, &plan.name, &canonical);
    properties.insert("Id", ElementValue::String(canonical.clone().into()));
    for (name, value) in values {
        properties.insert(name.as_str(), value.clone());
    }
    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new(source_id, &element_id),
            labels: vec![Arc::from(plan.name.as_str())].into(),
            effective_from: timestamp,
        },
        properties,
    };
    Ok(if insert {
        SourceChange::Insert { element }
    } else {
        SourceChange::Update { element }
    })
}

pub fn delete_change_with_org(
    source_id: &str,
    org_id: &str,
    plan: &ObjectPlan,
    id: &str,
    timestamp: u64,
) -> Result<SourceChange> {
    let canonical = plan.canonical_record_id(id)?;
    let element_id = graph_element_id(org_id, &plan.name, &canonical);
    Ok(SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new(source_id, &element_id),
            labels: vec![Arc::from(plan.name.as_str())].into(),
            effective_from: timestamp,
        },
    })
}

pub fn delete_change(plan: &ObjectPlan, id: &str, timestamp: u64) -> SourceChange {
    SourceChange::Delete {
        metadata: metadata(plan, id, timestamp),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::projection::properties_json;
    use serde_json::json;

    pub fn schema_json(decoder: &Decoder) -> String {
        decoder.avro.canonical_form()
    }

    pub fn fixture(
        operation: &str,
        changed: Vec<&str>,
        nulled: Vec<&str>,
        diffs: Vec<&str>,
        ids: Vec<&str>,
    ) -> (Decoder, Vec<u8>) {
        let change_types = [
            "CREATE",
            "UPDATE",
            "DELETE",
            "UNDELETE",
            "GAP_CREATE",
            "GAP_UPDATE",
            "GAP_DELETE",
            "GAP_UNDELETE",
            "GAP_OVERFLOW",
            "SNAPSHOT",
        ];
        let schema = json!({
            "type":"record", "name":"AccountChangeEvent", "fields":[
                {"name":"ChangeEventHeader","type":{"type":"record","name":"Header","fields":[
                    {"name":"entityName","type":"string"},
                    {"name":"changeType","type":{"type":"enum","name":"ChangeType","symbols":change_types}},
                    {"name":"recordIds","type":{"type":"array","items":"string"}},
                    {"name":"commitTimestamp","type":"long"},
                    {"name":"changedFields","type":{"type":"array","items":"string"}},
                    {"name":"nulledFields","type":{"type":"array","items":"string"}},
                    {"name":"diffFields","type":{"type":"array","items":"string"}}
                ]}},
                {"name":"Name","type":["null","string"]},
                {"name":"Phone","type":["null","string"]},
                {"name":"Description","type":["null","string"]},
                {"name":"IgnoredAddress","type":["null",{"type":"record","name":"Address","fields":[
                    {"name":"City","type":["null","string"]},
                    {"name":"Street","type":["null","string"]}
                ]}]}
            ]
        })
        .to_string();
        let decoder = Decoder::new(&schema, &ObjectPlan::account()).unwrap();
        let array = |items: Vec<&str>| {
            Avro::Array(items.into_iter().map(|s| Avro::String(s.into())).collect())
        };
        let value = Avro::Record(vec![
            (
                "ChangeEventHeader".into(),
                Avro::Record(vec![
                    ("entityName".into(), Avro::String("Account".into())),
                    (
                        "changeType".into(),
                        Avro::Enum(
                            change_types
                                .iter()
                                .position(|value| *value == operation)
                                .unwrap() as u32,
                            operation.into(),
                        ),
                    ),
                    ("recordIds".into(), array(ids)),
                    ("commitTimestamp".into(), Avro::Long(1_700_000_000_000)),
                    ("changedFields".into(), array(changed)),
                    ("nulledFields".into(), array(nulled)),
                    ("diffFields".into(), array(diffs)),
                ]),
            ),
            (
                "Name".into(),
                Avro::Union(1, Box::new(Avro::String("Example Account".into()))),
            ),
            ("Phone".into(), Avro::Union(0, Box::new(Avro::Null))),
            ("Description".into(), Avro::Union(0, Box::new(Avro::Null))),
            (
                "IgnoredAddress".into(),
                Avro::Union(0, Box::new(Avro::Null)),
            ),
        ]);
        let payload = apache_avro::to_avro_datum(&decoder.avro, value).unwrap();
        (decoder, payload)
    }

    #[test]
    fn change_type_header_uses_salesforce_enum_symbols() {
        for (symbol, operation) in [
            ("CREATE", Operation::Create),
            ("UPDATE", Operation::Update),
            ("DELETE", Operation::Delete),
            ("UNDELETE", Operation::Undelete),
        ] {
            let (decoder, payload) =
                fixture(symbol, vec![], vec![], vec![], vec!["001000000000001AAA"]);
            assert_eq!(decoder.decode(&payload).unwrap().operation, operation);
        }
    }

    #[test]
    fn unchanged_nulls_are_omitted_and_explicit_nulls_preserved() {
        let (decoder, payload) = fixture(
            "UPDATE",
            vec!["0x02"],
            vec!["0x04"],
            vec![],
            vec!["001000000000001AAA"],
        );
        let change = decoder.decode(&payload).unwrap();
        let properties = properties_json(&change.properties);
        assert_eq!(properties["Name"], "Example Account");
        assert_eq!(properties.get("Phone"), Some(&Value::Null));
        assert!(!change.properties.contains_key("Description"));
        assert_eq!(change.timestamp, 1_700_000_000_000);
    }

    #[test]
    fn creates_and_undeletes_do_not_require_changed_fields() {
        for operation in ["CREATE", "UNDELETE"] {
            let (decoder, payload) = fixture(
                operation,
                vec![],
                vec![],
                vec![],
                vec!["001000000000001AAA"],
            );
            assert_eq!(decoder.decode(&payload).unwrap().properties.len(), 3);
        }
    }

    #[test]
    fn merged_deletes_preserve_all_record_ids() {
        let ids = vec!["001000000000001AAA", "001000000000002AAA"];
        let (decoder, payload) = fixture("DELETE", vec![], vec![], vec![], ids.clone());
        let change = decoder.decode(&payload).unwrap();
        assert_eq!(change.ids, ids);
        assert_eq!(change.operation, Operation::Delete);
        assert!(change.properties.is_empty());
    }

    #[test]
    fn gaps_wildcards_and_selected_text_diffs_fail_explicitly() {
        for (operation, ids, diffs) in [
            ("GAP_UPDATE", vec!["001000000000001AAA"], vec![]),
            ("GAP_OVERFLOW", vec!["000000000000000AAA"], vec![]),
            ("UPDATE", vec!["001*"], vec![]),
            ("UPDATE", vec!["001000000000001AAA"], vec!["0x08"]),
        ] {
            let (decoder, payload) = fixture(operation, vec![], vec![], diffs, ids);
            assert!(decoder.decode(&payload).is_err());
        }
    }

    #[test]
    fn malformed_bitmaps_and_ambiguous_null_updates_are_rejected() {
        for changed in [vec!["0x04"], vec!["0x80"], vec!["0xZZ"]] {
            let (decoder, payload) = fixture(
                "UPDATE",
                changed,
                vec![],
                vec![],
                vec!["001000000000001AAA"],
            );
            assert!(decoder.decode(&payload).is_err());
        }
    }

    #[test]
    fn unrelated_compound_bitmaps_do_not_clear_projected_scalar_fields() {
        let (decoder, payload) = fixture(
            "UPDATE",
            vec!["0x00", "4-0x02"],
            vec!["4-0x01"],
            vec![],
            vec!["001000000000001AAA"],
        );
        assert!(decoder.decode(&payload).unwrap().properties.is_empty());
    }

    #[test]
    fn rest_timestamp_formats_have_the_same_effective_time() {
        for timestamp in [
            "2023-11-14T22:13:20.000Z",
            "2023-11-14T22:13:20.000+0000",
            "2023-11-14T22:13:20.000+00:00",
        ] {
            let plan = ObjectPlan::account();
            let record = plan
                .snapshot(&json!({
                    "Id":"001000000000001AAA", "Name":"Example Account",
                    "Phone":null, "Description":null, "LastModifiedDate":timestamp
                }))
                .unwrap();
            assert_eq!(
                record.change(&plan).get_transaction_time(),
                1_700_000_000_000
            );
        }
    }

    #[test]
    fn compound_bitmaps_update_and_clear_only_selected_components() -> Result<()> {
        let (base, payload) = fixture(
            "UPDATE",
            vec!["4-0x01"],
            vec!["4-0x02"],
            vec![],
            vec!["001000000000001AAA"],
        );
        let mut plan = ObjectPlan::account();
        plan.fields.push(Field {
            name: "IgnoredAddress".into(),
            rest_name: "IgnoredAddress".into(),
            nullable: true,
            kind: Kind::Compound(
                ["City", "Street"]
                    .into_iter()
                    .map(|name| Field {
                        name: name.into(),
                        rest_name: name.into(),
                        kind: Kind::Text,
                        nullable: true,
                    })
                    .collect(),
            ),
        });
        let decoder = Decoder::new(&schema_json(&base), &plan)?;
        let mut remaining = payload.as_slice();
        let Avro::Record(mut record) =
            apache_avro::from_avro_datum(&base.avro, &mut remaining, None)?
        else {
            bail!("Fixture is not a record");
        };
        record
            .iter_mut()
            .find(|(name, _)| name == "IgnoredAddress")
            .unwrap()
            .1 = Avro::Union(
            1,
            Box::new(Avro::Record(vec![
                (
                    "City".into(),
                    Avro::Union(1, Box::new(Avro::String("New city".into()))),
                ),
                ("Street".into(), Avro::Union(0, Box::new(Avro::Null))),
            ])),
        );
        let payload = apache_avro::to_avro_datum(&decoder.avro, Avro::Record(record))?;
        let change = decoder.decode(&payload)?;
        assert_eq!(
            properties_json(&change.properties)["IgnoredAddress"],
            json!({"City":"New city","Street":null})
        );
        Ok(())
    }

    #[test]
    fn salesforce_text_diffs_are_reconstructed_and_hash_checked() -> Result<()> {
        let old = "first line\nold line\nlast line\n";
        let new = "first line\nchanged line\nlast line\n";
        let patch = diffy::create_patch(old, new).to_string();
        let hunks = patch.splitn(3, '\n').nth(2).unwrap();
        let hash = format!("{:x}", Sha256::digest(new.as_bytes()));
        let salesforce = format!("--- \n+++ {hash}\n{hunks}");
        assert_eq!(apply_text_diff(old, &salesforce)?, new);
        assert!(apply_text_diff("wrong baseline\n", &salesforce).is_err());
        let corrupted = salesforce.replace(&hash, &"0".repeat(64));
        assert!(apply_text_diff(old, &corrupted).is_err());
        assert!(apply_text_diff(old, "not a unified diff").is_err());
        Ok(())
    }

    #[test]
    fn text_diff_preserves_line_endings_and_final_newline_state() -> Result<()> {
        for (old, new) in [
            ("first\nold\nlast", "first\nnew\nlast"),
            ("first\r\nold\r\nlast\r\n", "first\r\nnew\r\nlast\r\n"),
            ("old", "new\n"),
        ] {
            let patch = diffy::create_patch(old, new).to_string();
            let hunks = patch.splitn(3, '\n').nth(2).unwrap();
            let patch = format!("--- \n+++ {:x}\n{hunks}", Sha256::digest(new.as_bytes()));
            assert_eq!(apply_text_diff(old, &patch)?, new);
        }
        Ok(())
    }

    #[test]
    fn native_salesforce_diff_without_patch_terminator_preserves_lines() -> Result<()> {
        let old = "first\nold\nlast";
        let expected = "first\nnew\nlast";
        let patch = format!(
            "--- \n+++ {:x}\n@@ -2,1 +2,1 @@\n-old\n+new",
            Sha256::digest(expected.as_bytes())
        );
        assert_eq!(apply_text_diff(old, &patch)?, expected);
        Ok(())
    }

    #[test]
    fn invalidation_decoder_extracts_ids_without_field_projection() {
        let (decoder, payload) = fixture(
            "UPDATE",
            vec!["0x02"],
            vec!["0x04"],
            vec!["0x08"],
            vec!["001000000000001AAA"],
        );
        let invalidation = decoder.decode_invalidation(&payload).unwrap();
        assert_eq!(invalidation.operation, Operation::Update);
        assert_eq!(invalidation.ids, vec!["001000000000001AAA"]);
        assert_eq!(invalidation.timestamp, 1_700_000_000_000);
    }

    #[test]
    fn invalidation_accepts_id_and_rest_only_fields_absent_from_native_schema() -> Result<()> {
        let (fixture_decoder, payload) =
            fixture("UPDATE", vec![], vec![], vec![], vec!["001000000000001AAA"]);
        let schema = schema_json(&fixture_decoder);
        let mut plan = ObjectPlan::account();
        for name in ["Id", "Formula__c"] {
            plan.fields.push(Field {
                name: name.into(),
                rest_name: name.into(),
                kind: Kind::Text,
                nullable: true,
            });
        }
        assert!(Decoder::new(&schema, &plan).is_err());
        let invalidation =
            Decoder::for_invalidations(&schema, &plan)?.decode_invalidation(&payload)?;
        assert_eq!(invalidation.ids, vec!["001000000000001AAA"]);
        Ok(())
    }

    #[test]
    fn native_gaps_request_full_reconciliation_without_record_ids() -> Result<()> {
        for operation in [
            "GAP_CREATE",
            "GAP_UPDATE",
            "GAP_DELETE",
            "GAP_UNDELETE",
            "GAP_OVERFLOW",
            "SNAPSHOT",
        ] {
            let (decoder, payload) = fixture(operation, vec![], vec![], vec![], vec![]);
            let invalidation = decoder.decode_invalidation(&payload)?;
            assert_eq!(invalidation.operation, Operation::Reconcile);
            assert!(invalidation.ids.is_empty());
            assert!(decoder.decode(&payload).is_err());
        }
        Ok(())
    }
}
