use anyhow::{anyhow, bail, ensure, Context, Result};
use apache_avro::types::Value as Avro;
use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use drasi_core::models::{ElementPropertyMap, ElementValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub type Properties = BTreeMap<String, ElementValue>;

#[derive(Clone, Debug)]
pub enum Kind {
    Text,
    Bool,
    Integer,
    Float,
    Date,
    Time,
    Instant,
    MultiSelect,
    Compound(Vec<Field>),
}

#[derive(Clone, Debug)]
pub struct Field {
    pub name: String,
    pub rest_name: String,
    pub kind: Kind,
    pub nullable: bool,
}

#[derive(Clone, Debug)]
pub struct ObjectPlan {
    pub name: String,
    pub key_prefix: String,
    pub fields: Vec<Field>,
}

#[derive(Clone, Debug)]
pub struct Record {
    pub id: String,
    pub properties: Properties,
    pub timestamp: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Describe {
    pub name: String,
    #[serde(default = "default_true")]
    pub queryable: bool,
    pub key_prefix: String,
    pub fields: Vec<DescribeField>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DescribeField {
    pub name: String,
    #[serde(rename = "type")]
    pub field_type: String,
    #[serde(default = "default_true")]
    pub queryable: bool,
    pub nillable: bool,
    #[serde(default)]
    pub relationship_name: Option<String>,
    #[serde(default)]
    pub reference_to: Vec<String>,
    #[serde(default)]
    pub deprecated_and_hidden: bool,
    pub compound_field_name: Option<String>,
}

impl ObjectPlan {
    pub fn account() -> Self {
        Self {
            name: "Account".into(),
            key_prefix: "001".into(),
            fields: ["Name", "Phone", "Description"]
                .into_iter()
                .map(|name| Field {
                    name: name.into(),
                    rest_name: name.into(),
                    kind: Kind::Text,
                    nullable: name != "Name",
                })
                .collect(),
        }
    }

    pub fn from_describe(describe: Describe, selected: &[String]) -> Result<Self> {
        validate_identifier(&describe.name)?;
        ensure!(
            describe.key_prefix.len() == 3
                && describe
                    .key_prefix
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric()),
            "Invalid object key prefix"
        );
        let mut fields = Vec::new();
        for name in selected {
            validate_identifier(name)?;
            let field = describe
                .fields
                .iter()
                .find(|field| &field.name == name)
                .with_context(|| format!("Selected field {name} is absent from Describe"))?;
            let kind = if matches!(field.field_type.as_str(), "address" | "location") {
                let prefix = format!("{}__", name.trim_end_matches("__c"));
                let parts = describe
                    .fields
                    .iter()
                    .filter(|part| part.compound_field_name.as_deref() == Some(name))
                    .map(|part| {
                        let component = part
                            .name
                            .strip_prefix(&prefix)
                            .and_then(|value| value.strip_suffix("__s"))
                            .with_context(|| {
                                format!("Unexpected compound component {}", part.name)
                            })?;
                        Ok(Field {
                            name: component.into(),
                            rest_name: part.name.clone(),
                            kind: scalar_kind(&part.field_type)?,
                            nullable: part.nillable,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                ensure!(
                    !parts.is_empty(),
                    "Compound field {name} has no described components"
                );
                Kind::Compound(parts)
            } else {
                scalar_kind(&field.field_type)?
            };
            fields.push(Field {
                name: name.clone(),
                rest_name: name.clone(),
                kind,
                nullable: field.nillable,
            });
        }
        Ok(Self {
            name: describe.name,
            key_prefix: describe.key_prefix,
            fields,
        })
    }

    pub fn topic(&self) -> String {
        if let Some(name) = self.name.strip_suffix("__c") {
            format!("/data/{name}__ChangeEvent")
        } else {
            format!("/data/{}ChangeEvent", self.name)
        }
    }

    pub fn projection(&self) -> String {
        let mut names = vec!["Id".to_owned()];
        for field in &self.fields {
            match &field.kind {
                Kind::Compound(parts) => {
                    names.extend(parts.iter().map(|part| part.rest_name.clone()))
                }
                _ => names.push(field.rest_name.clone()),
            }
        }
        names.sort();
        names.dedup();
        names.join(", ")
    }

    pub fn validate_id(&self, id: &str) -> Result<()> {
        let canonical = canonical_record_id(id)?;
        ensure!(
            canonical.starts_with(&self.key_prefix),
            "Expected a {} record ID, not a wildcard or overflow marker",
            self.name
        );
        Ok(())
    }

    pub fn canonical_record_id(&self, id: &str) -> Result<String> {
        self.validate_id(id)?;
        canonical_record_id(id)
    }

    pub fn snapshot(&self, value: &Value) -> Result<Record> {
        let id = value["Id"].as_str().context("Record has no Id")?.to_owned();
        let id = self.canonical_record_id(&id)?;
        let timestamp = extract_observed_timestamp(value)?.timestamp_millis();
        let properties = self
            .fields
            .iter()
            .map(|field| Ok((field.name.clone(), field.decode_rest(value)?)))
            .collect::<Result<_>>()?;
        Ok(Record {
            id,
            properties,
            timestamp: u64::try_from(timestamp)?,
        })
    }

    pub fn query(&self) -> String {
        let columns = std::iter::once("a.Id AS id".to_owned())
            .chain(self.fields.iter().map(|field| {
                let alias = if self.name == "Account" {
                    field.name.to_ascii_lowercase()
                } else {
                    field.name.clone()
                };
                format!("a.{} AS {alias}", field.name)
            }))
            .collect::<Vec<_>>()
            .join(", ");
        format!("MATCH (a:{}) RETURN {columns}", self.name)
    }

    pub fn graph_element_id(&self, org_id: &str, canonical_record_id: &str) -> Result<String> {
        validate_org_id(org_id)?;
        let canonical = self.canonical_record_id(canonical_record_id)?;
        Ok(graph_element_id(org_id, &self.name, &canonical))
    }
}

pub fn graph_element_id(org_id: &str, object: &str, canonical_record_id: &str) -> String {
    format!("sf:{org_id}:{object}:{canonical_record_id}")
}

pub fn validate_org_id(org_id: &str) -> Result<()> {
    ensure!(
        matches!(org_id.len(), 15 | 18) && org_id.bytes().all(|b| b.is_ascii_alphanumeric()),
        "Invalid Salesforce organization identifier"
    );
    Ok(())
}

pub fn canonical_record_id(id: &str) -> Result<String> {
    ensure!(
        matches!(id.len(), 15 | 18) && id.bytes().all(|b| b.is_ascii_alphanumeric()),
        "Invalid Salesforce record ID"
    );

    if id.len() == 18 {
        return Ok(id.to_owned());
    }

    let mut suffix = String::with_capacity(3);
    for chunk in id.as_bytes().chunks(5) {
        let mut bitset = 0u8;
        for (index, byte) in chunk.iter().enumerate() {
            if byte.is_ascii_uppercase() {
                bitset |= 1 << index;
            }
        }
        suffix.push(checksum_char(bitset)?);
    }

    Ok(format!("{id}{suffix}"))
}

fn checksum_char(bitset: u8) -> Result<char> {
    const CHARS: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ012345";
    CHARS
        .get(bitset as usize)
        .copied()
        .map(char::from)
        .ok_or_else(|| anyhow!("Invalid Salesforce ID checksum bitset"))
}

impl Field {
    pub fn empty_compound(&self) -> Result<ElementValue> {
        let Kind::Compound(parts) = &self.kind else {
            bail!("Expected compound field");
        };
        Ok(object(
            parts
                .iter()
                .map(|part| (part.name.clone(), ElementValue::Null)),
        ))
    }

    pub fn decode_rest(&self, row: &Value) -> Result<ElementValue> {
        if let Kind::Compound(parts) = &self.kind {
            return Ok(object(
                parts
                    .iter()
                    .map(|part| Ok((part.name.clone(), part.decode_rest(row)?)))
                    .collect::<Result<Vec<_>>>()?,
            ));
        }
        let value = row
            .get(&self.rest_name)
            .with_context(|| format!("REST omitted selected field {}", self.rest_name))?;
        if value.is_null() {
            return self.null();
        }
        match &self.kind {
            Kind::Text => Ok(ElementValue::String(
                value.as_str().context("Expected REST text")?.into(),
            )),
            Kind::Bool => Ok(ElementValue::Bool(
                value.as_bool().context("Expected REST boolean")?,
            )),
            Kind::Integer => Ok(ElementValue::Integer(
                value.as_i64().context("Expected REST integer")?,
            )),
            Kind::Float => finite_float(value.as_f64().context("Expected REST number")?),
            Kind::Date => Ok(ElementValue::String(
                NaiveDate::parse_from_str(
                    value.as_str().context("Expected REST date")?,
                    "%Y-%m-%d",
                )?
                .to_string()
                .into(),
            )),
            Kind::Time => {
                let time = value
                    .as_str()
                    .context("Expected REST time")?
                    .trim_end_matches('Z');
                let time = NaiveTime::parse_from_str(time, "%H:%M:%S%.f")?;
                Ok(ElementValue::String(
                    time.format("%H:%M:%S%.3f").to_string().into(),
                ))
            }
            Kind::Instant => Ok(ElementValue::ZonedDateTime(
                parse_instant(value.as_str().context("Expected REST datetime")?)?.fixed_offset(),
            )),
            Kind::MultiSelect => Ok(selections(
                value.as_str().context("Expected REST multi-select")?,
            )),
            Kind::Compound(_) => unreachable!(),
        }
    }

    pub fn decode_avro(&self, value: &Avro) -> Result<ElementValue> {
        let value = unbox(value);
        if matches!(value, Avro::Null) {
            return self.null();
        }
        match (&self.kind, value) {
            (Kind::Text, Avro::String(text) | Avro::Enum(_, text)) => {
                Ok(ElementValue::String(text.as_str().into()))
            }
            (Kind::Bool, Avro::Boolean(value)) => Ok(ElementValue::Bool(*value)),
            (Kind::Integer, Avro::Int(value)) => Ok(ElementValue::Integer(i64::from(*value))),
            (Kind::Integer, Avro::Long(value)) => Ok(ElementValue::Integer(*value)),
            (Kind::Float, Avro::Double(value)) => finite_float(*value),
            (Kind::Float, Avro::Float(value)) => finite_float(f64::from(*value)),
            (Kind::Date, Avro::Long(value)) => Ok(ElementValue::String(
                DateTime::from_timestamp_millis(*value)
                    .context("Date is out of range")?
                    .date_naive()
                    .to_string()
                    .into(),
            )),
            (Kind::Instant, Avro::Long(value)) => Ok(ElementValue::ZonedDateTime(
                DateTime::from_timestamp_millis(*value)
                    .context("Datetime is out of range")?
                    .fixed_offset(),
            )),
            (Kind::Time, Avro::Long(value)) => {
                ensure!((0..86_400_000).contains(value), "Time is outside one day");
                let millis = u32::try_from(*value)?;
                let time = NaiveTime::from_num_seconds_from_midnight_opt(
                    millis / 1000,
                    (millis % 1000) * 1_000_000,
                )
                .context("Invalid time")?;
                Ok(ElementValue::String(
                    time.format("%H:%M:%S%.3f").to_string().into(),
                ))
            }
            (Kind::MultiSelect, Avro::String(text)) => Ok(selections(text)),
            _ => bail!(
                "Unsupported native value for field {} ({:?})",
                self.name,
                self.kind
            ),
        }
    }

    pub fn null(&self) -> Result<ElementValue> {
        ensure!(self.nullable, "Non-nullable field {} was null", self.name);
        Ok(ElementValue::Null)
    }
}

pub fn unbox(value: &Avro) -> &Avro {
    match value {
        Avro::Union(_, inner) => unbox(inner),
        value => value,
    }
}

pub fn object(values: impl IntoIterator<Item = (String, ElementValue)>) -> ElementValue {
    let mut result = ElementPropertyMap::new();
    for (key, value) in values {
        result.insert(&key, value);
    }
    ElementValue::Object(result)
}

pub fn merge_properties(previous: &mut Properties, patch: Properties) {
    for (name, mut value) in patch {
        if let (Some(ElementValue::Object(old)), ElementValue::Object(new)) =
            (previous.get(&name), &mut value)
        {
            new.merge(old);
        }
        previous.insert(name, value);
    }
}

pub fn properties_json(properties: &Properties) -> serde_json::Map<String, Value> {
    properties
        .iter()
        .map(|(name, value)| (name.clone(), query_json(value)))
        .collect()
}

pub fn query_json(value: &ElementValue) -> Value {
    match value {
        ElementValue::ZonedDateTime(value) => Value::String(value.to_rfc3339()),
        ElementValue::Object(map) => Value::Object(
            map.map_iter(|key, value| (key.to_string(), query_json(value)))
                .collect(),
        ),
        ElementValue::List(values) => Value::Array(values.iter().map(query_json).collect()),
        value => Value::from(value),
    }
}

pub fn parse_instant(value: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f%#z")?.with_timezone(&Utc))
}

fn extract_observed_timestamp(value: &Value) -> Result<DateTime<Utc>> {
    for field in ["LastModifiedDate", "SystemModstamp", "CreatedDate"] {
        if let Some(timestamp) = value.get(field).and_then(Value::as_str) {
            return parse_instant(timestamp)
                .with_context(|| format!("invalid Salesforce timestamp field {field}"));
        }
    }
    Ok(Utc::now())
}

pub fn validate_identifier(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
            && value.as_bytes()[0].is_ascii_alphabetic(),
        "Invalid Salesforce API identifier"
    );
    Ok(())
}

fn finite_float(value: f64) -> Result<ElementValue> {
    ensure!(value.is_finite(), "Non-finite Salesforce number");
    Ok(ElementValue::Float(value.into()))
}

fn selections(value: &str) -> ElementValue {
    let mut values = if value.is_empty() {
        Vec::new()
    } else {
        value.split(';').collect()
    };
    values.sort_unstable();
    ElementValue::List(
        values
            .into_iter()
            .map(|value| ElementValue::String(value.into()))
            .collect(),
    )
}

pub(crate) fn scalar_kind(value: &str) -> Result<Kind> {
    Ok(match value {
        "string" | "textarea" | "email" | "phone" | "url" | "picklist" | "reference" | "id"
        | "encryptedstring" => Kind::Text,
        "boolean" => Kind::Bool,
        "int" => Kind::Integer,
        "double" | "currency" | "percent" => Kind::Float,
        "date" => Kind::Date,
        "time" => Kind::Time,
        "datetime" => Kind::Instant,
        "multipicklist" => Kind::MultiSelect,
        other => bail!("Unsupported Salesforce field type {other}"),
    })
}

const fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn field(kind: Kind) -> Field {
        Field {
            name: "Value".into(),
            rest_name: "Value".into(),
            kind,
            nullable: true,
        }
    }

    #[test]
    fn rest_and_native_values_have_the_same_types_and_values() -> Result<()> {
        let cases = [
            (
                Kind::Text,
                json!("Unicode \u{2603}\nline"),
                Avro::String("Unicode \u{2603}\nline".into()),
                ElementValue::String("Unicode \u{2603}\nline".into()),
            ),
            (
                Kind::Bool,
                json!(false),
                Avro::Boolean(false),
                ElementValue::Bool(false),
            ),
            (
                Kind::Integer,
                json!(i64::MAX),
                Avro::Long(i64::MAX),
                ElementValue::Integer(i64::MAX),
            ),
            (
                Kind::Integer,
                json!(i32::MIN),
                Avro::Int(i32::MIN),
                ElementValue::Integer(i64::from(i32::MIN)),
            ),
            (
                Kind::Float,
                json!(0),
                Avro::Double(0.0),
                ElementValue::Float(0.0.into()),
            ),
            (
                Kind::Float,
                json!(-1234.56789),
                Avro::Double(-1234.56789),
                ElementValue::Float((-1234.56789).into()),
            ),
            (
                Kind::Float,
                json!(9_007_199_254_740_991_i64),
                Avro::Double(9_007_199_254_740_991.0),
                ElementValue::Float(9_007_199_254_740_991.0.into()),
            ),
            (
                Kind::Date,
                json!("1969-12-31"),
                Avro::Long(-86_400_000),
                ElementValue::String("1969-12-31".into()),
            ),
            (
                Kind::Date,
                json!("1970-01-02"),
                Avro::Long(86_400_000),
                ElementValue::String("1970-01-02".into()),
            ),
            (
                Kind::Time,
                json!("23:59:59.999Z"),
                Avro::Long(86_399_999),
                ElementValue::String("23:59:59.999".into()),
            ),
            (
                Kind::Time,
                json!("00:00:00.001Z"),
                Avro::Long(1),
                ElementValue::String("00:00:00.001".into()),
            ),
            (
                Kind::Instant,
                json!("1970-01-01T01:00:00.001+01:00"),
                Avro::Long(1),
                ElementValue::ZonedDateTime(
                    DateTime::from_timestamp_millis(1).unwrap().fixed_offset(),
                ),
            ),
            (
                Kind::MultiSelect,
                json!("Gamma;Alpha"),
                Avro::String("Gamma;Alpha".into()),
                ElementValue::List(vec![
                    ElementValue::String("Alpha".into()),
                    ElementValue::String("Gamma".into()),
                ]),
            ),
            (
                Kind::MultiSelect,
                json!(""),
                Avro::String(String::new()),
                ElementValue::List(vec![]),
            ),
        ];
        for (kind, rest, native, expected) in cases {
            let field = field(kind);
            assert_eq!(
                field.decode_rest(&json!({"Value": rest}))?,
                expected,
                "REST {:?}",
                field.kind
            );
            assert_eq!(
                field.decode_avro(&native)?,
                expected,
                "Avro {:?}",
                field.kind
            );
        }
        Ok(())
    }

    #[test]
    fn invalid_numbers_times_and_nonnullable_nulls_fail() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(field(Kind::Float)
                .decode_avro(&Avro::Double(value))
                .is_err());
        }
        for value in [-1, 86_400_000] {
            assert!(field(Kind::Time).decode_avro(&Avro::Long(value)).is_err());
        }
        let mut required = field(Kind::Bool);
        required.nullable = false;
        assert!(required.decode_rest(&json!({"Value":null})).is_err());
        assert!(required.decode_avro(&Avro::Null).is_err());
        assert!(field(Kind::Date)
            .decode_rest(&json!({"Value":"2023-02-29"}))
            .is_err());
        assert!(field(Kind::Instant)
            .decode_rest(&json!({"Value":"2024-01-01T12:00:00"}))
            .is_err());
    }

    #[test]
    fn compound_patch_preserves_untouched_components() {
        let mut previous = Properties::from([(
            "Geo".into(),
            object([
                ("Latitude".into(), ElementValue::Float(1.25.into())),
                ("Longitude".into(), ElementValue::Float(2.5.into())),
            ]),
        )]);
        merge_properties(
            &mut previous,
            Properties::from([(
                "Geo".into(),
                object([("Longitude".into(), ElementValue::Null)]),
            )]),
        );
        assert_eq!(
            properties_json(&previous)["Geo"],
            json!({"Latitude":1.25,"Longitude":null})
        );
    }

    #[test]
    fn float_contract_does_not_claim_exact_decimal_or_large_integer_precision() {
        assert_eq!(9_007_199_254_740_992_f64, 9_007_199_254_740_993_f64);
        assert_ne!(0.1_f64 + 0.2, 0.3);
    }

    #[test]
    fn fifteen_character_ids_are_canonicalized_to_eighteen_characters() -> Result<()> {
        let object = ObjectPlan::account();
        assert_eq!(
            object.canonical_record_id("001000000000001")?,
            "001000000000001AAA"
        );
        assert_eq!(
            canonical_record_id("001A0000009z3ZI")?,
            "001A0000009z3ZIIAY"
        );
        Ok(())
    }

    #[test]
    fn snapshot_uses_observation_time_when_timestamp_fields_are_absent() -> Result<()> {
        let plan = ObjectPlan::account();
        let before = Utc::now().timestamp_millis();
        let record = plan.snapshot(&json!({
            "Id":"001000000000001AAA",
            "Name":"Example Account",
            "Phone":null,
            "Description":null
        }))?;
        let after = Utc::now().timestamp_millis();
        let observed = i64::try_from(record.timestamp)?;
        assert!(
            observed >= before && observed <= after,
            "observation timestamp {observed} not in expected range [{before}, {after}]"
        );
        Ok(())
    }

    #[test]
    fn graph_ids_are_stable_and_include_org_and_object() -> Result<()> {
        let object = ObjectPlan::account();
        let graph_id = object.graph_element_id("00D000000000001AAA", "001000000000001AAA")?;
        assert_eq!(graph_id, "sf:00D000000000001AAA:Account:001000000000001AAA");
        Ok(())
    }

    #[test]
    fn invalid_ids_are_rejected() {
        assert!(canonical_record_id("001*").is_err());
        assert!(canonical_record_id("001000000000000AAA!").is_err());
        assert!(validate_org_id("invalid-org").is_err());
    }
}
