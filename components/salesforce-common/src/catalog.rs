use anyhow::{bail, ensure, Context, Result};
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use drasi_lib::schema::{NodeSchema, PropertySchema, PropertyType, RelationSchema, SourceSchema};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use crate::projection::{
    canonical_record_id, graph_element_id, scalar_kind, validate_identifier, validate_org_id,
    Describe, Kind, ObjectPlan, Properties, Record,
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredSchema {
    pub fingerprint: String,
    pub objects: Vec<ObjectSchema>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ObjectSchema {
    pub object: String,
    pub queryable: bool,
    pub key_prefix: String,
    pub fields: Vec<FieldSchema>,
    pub lookups: Vec<LookupSchema>,
    pub omissions: Vec<SchemaOmission>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FieldSchema {
    pub name: String,
    pub field_type: String,
    pub nullable: bool,
    pub compound_field_name: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LookupSchema {
    pub field: String,
    pub label: String,
    pub reference_to: Vec<String>,
    pub selected_targets: Vec<String>,
    pub selected_target_key_prefixes: Vec<String>,
    pub omitted_targets: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SchemaOmission {
    pub object: String,
    pub field: String,
    pub reason: String,
}

#[derive(Clone, Debug)]
pub struct Catalog {
    pub plans: Vec<ObjectPlan>,
    pub schema: DiscoveredSchema,
    plans_by_object: HashMap<String, CatalogPlan>,
}

#[derive(Clone, Debug)]
struct CatalogPlan {
    plan_index: usize,
    lookups: Vec<LookupPlan>,
    schema_revision: String,
}

#[derive(Clone, Debug)]
struct LookupPlan {
    field: String,
    label: String,
    candidates: Vec<LookupTarget>,
}

#[derive(Clone, Debug)]
struct LookupTarget {
    object: String,
    key_prefix: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GraphRecord {
    pub object: String,
    pub id: String,
    pub properties: Properties,
    pub relationships: Vec<LookupRelationship>,
    pub observed_at_ms: u64,
    pub schema_revision: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LookupRelationship {
    pub field: String,
    pub label: String,
    pub target_object: String,
    pub target_id: String,
}

impl Catalog {
    /// Describe the emitted graph using the same typed plans as record projection.
    pub fn source_schema(&self) -> SourceSchema {
        let mut nodes = self
            .plans
            .iter()
            .map(|plan| {
                let mut properties = plan
                    .fields
                    .iter()
                    .map(|field| {
                        let (data_type, description) = match &field.kind {
                            Kind::Text => (PropertyType::String, None),
                            Kind::Bool => (PropertyType::Boolean, None),
                            Kind::Integer => (PropertyType::Integer, None),
                            Kind::Float => (PropertyType::Float, None),
                            Kind::Date => (PropertyType::String, Some("Date (YYYY-MM-DD)")),
                            Kind::Time => (PropertyType::String, Some("Time (HH:mm:ss.SSS)")),
                            Kind::Instant => (PropertyType::Timestamp, None),
                            Kind::MultiSelect => {
                                (PropertyType::Json, Some("Sorted list of strings"))
                            }
                            Kind::Compound(_) => (PropertyType::Json, None),
                        };
                        (
                            field.name.clone(),
                            PropertySchema {
                                name: field.name.clone(),
                                data_type: Some(data_type),
                                description: description.map(str::to_owned),
                            },
                        )
                    })
                    .collect::<BTreeMap<_, _>>();
                properties.insert("Id".to_string(), string_property("Id"));
                NodeSchema {
                    label: plan.name.clone(),
                    properties: properties.into_values().collect(),
                }
            })
            .collect::<Vec<_>>();
        nodes.sort_by(|left, right| left.label.cmp(&right.label));

        let mut endpoints = BTreeMap::new();
        for object in &self.schema.objects {
            for lookup in &object.lookups {
                if lookup.selected_targets.is_empty() {
                    continue;
                }
                let (owners, targets) = endpoints
                    .entry(lookup.label.as_str())
                    .or_insert_with(|| (BTreeSet::new(), BTreeSet::new()));
                owners.insert(object.object.as_str());
                targets.extend(lookup.selected_targets.iter().map(String::as_str));
            }
        }
        let relations = endpoints
            .into_iter()
            .map(|(label, (owners, targets))| RelationSchema {
                label: label.to_string(),
                from: unique_label(&owners),
                to: unique_label(&targets),
                properties: ["Field", "TargetId", "TargetObject"]
                    .into_iter()
                    .map(string_property)
                    .collect(),
            })
            .collect();
        SourceSchema { nodes, relations }
    }

    pub fn from_describes(describes: Vec<Describe>) -> Result<Catalog> {
        ensure!(
            !describes.is_empty(),
            "at least one Salesforce object Describe is required"
        );

        let mut describe_by_object = BTreeMap::new();
        for describe in describes {
            validate_identifier(&describe.name)
                .with_context(|| format!("invalid Salesforce object name '{}'", describe.name))?;
            ensure!(
                describe.queryable,
                "Salesforce object '{}' is not queryable",
                describe.name
            );
            ensure!(
                describe.key_prefix.len() == 3
                    && describe
                        .key_prefix
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric()),
                "Salesforce object '{}' has invalid keyPrefix '{}'",
                describe.name,
                describe.key_prefix
            );
            ensure!(
                describe_by_object
                    .insert(describe.name.clone(), describe)
                    .is_none(),
                "duplicate Describe payload for Salesforce object"
            );
        }

        let mut key_prefix_index = HashMap::new();
        for (object, describe) in &describe_by_object {
            if let Some(existing) =
                key_prefix_index.insert(describe.key_prefix.clone(), object.clone())
            {
                bail!(
                    "Salesforce objects '{}' and '{}' share keyPrefix '{}', which is ambiguous",
                    existing,
                    object,
                    describe.key_prefix
                );
            }
        }

        let key_prefix_by_object = describe_by_object
            .iter()
            .map(|(object, describe)| (object.clone(), describe.key_prefix.clone()))
            .collect::<HashMap<_, _>>();

        let mut plans = Vec::new();
        let mut plans_by_object = HashMap::new();
        let mut schema_objects = Vec::new();

        for (object, describe) in &describe_by_object {
            let mut selected_fields = Vec::new();
            let mut field_schemas = Vec::new();
            let mut lookup_schemas = Vec::new();
            let mut omissions = Vec::new();
            let mut lookup_plans = Vec::new();
            let mut seen_fields = BTreeSet::new();

            for field in &describe.fields {
                validate_identifier(&field.name).with_context(|| {
                    format!(
                        "Salesforce object '{}' contains invalid field name '{}'",
                        object, field.name
                    )
                })?;
                ensure!(
                    seen_fields.insert(field.name.clone()),
                    "Salesforce object '{}' contains duplicate field '{}'",
                    object,
                    field.name
                );

                if field.deprecated_and_hidden {
                    omissions.push(SchemaOmission {
                        object: object.clone(),
                        field: field.name.clone(),
                        reason: "deprecatedAndHidden".to_string(),
                    });
                    continue;
                }

                if !field.queryable {
                    omissions.push(SchemaOmission {
                        object: object.clone(),
                        field: field.name.clone(),
                        reason: "fieldNotQueryable".to_string(),
                    });
                    continue;
                }

                if matches!(field.field_type.as_str(), "address" | "location") {
                    omissions.push(SchemaOmission {
                        object: object.clone(),
                        field: field.name.clone(),
                        reason: "unsupportedCompoundParentType".to_string(),
                    });
                    continue;
                }

                let kind = match scalar_kind(&field.field_type) {
                    Ok(kind) => kind,
                    Err(_) => {
                        omissions.push(SchemaOmission {
                            object: object.clone(),
                            field: field.name.clone(),
                            reason: format!("unsupportedFieldType:{}", field.field_type),
                        });
                        continue;
                    }
                };

                selected_fields.push(field.name.clone());
                field_schemas.push(FieldSchema {
                    name: field.name.clone(),
                    field_type: kind_name(&kind).to_string(),
                    nullable: field.nillable,
                    compound_field_name: field.compound_field_name.clone(),
                });

                if field.field_type != "reference" {
                    continue;
                }

                ensure!(
                    !field.reference_to.is_empty(),
                    "Salesforce object '{}' field '{}' has no reference targets",
                    object,
                    field.name
                );

                let label = match field
                    .relationship_name
                    .clone()
                    .filter(|label| !label.is_empty())
                {
                    Some(label) => label,
                    None => {
                        omissions.push(SchemaOmission {
                            object: object.clone(),
                            field: field.name.clone(),
                            reason: "missingRelationshipName".to_string(),
                        });
                        continue;
                    }
                };
                validate_identifier(&label).with_context(|| {
                    format!(
                        "Salesforce object '{}' field '{}' has invalid relationship label '{}'",
                        object, field.name, label
                    )
                })?;

                let mut seen_targets = BTreeSet::new();
                let mut selected_targets = Vec::new();
                let mut omitted_targets = Vec::new();
                let mut selected_target_key_prefixes = Vec::new();
                let mut candidates = Vec::new();
                let mut key_prefix_owner = HashMap::new();

                for target in &field.reference_to {
                    validate_identifier(target).with_context(|| {
                        format!(
                            "Salesforce object '{}' field '{}' has invalid reference target '{}'",
                            object, field.name, target
                        )
                    })?;
                    ensure!(
                        seen_targets.insert(target.clone()),
                        "Salesforce object '{}' field '{}' has duplicate reference target '{}'",
                        object,
                        field.name,
                        target
                    );

                    match key_prefix_by_object.get(target) {
                        Some(prefix) => {
                            if let Some(existing) =
                                key_prefix_owner.insert(prefix.clone(), target.clone())
                            {
                                bail!(
                                    "Salesforce object '{}' field '{}' has ambiguous polymorphic targets '{}' and '{}' sharing keyPrefix '{}'",
                                    object,
                                    field.name,
                                    existing,
                                    target,
                                    prefix
                                );
                            }
                            selected_targets.push(target.clone());
                            selected_target_key_prefixes.push(prefix.clone());
                            candidates.push(LookupTarget {
                                object: target.clone(),
                                key_prefix: prefix.clone(),
                            });
                        }
                        None => omitted_targets.push(target.clone()),
                    }
                }

                selected_targets.sort();
                omitted_targets.sort();
                selected_target_key_prefixes.sort();
                candidates.sort_by(|left, right| left.object.cmp(&right.object));

                lookup_schemas.push(LookupSchema {
                    field: field.name.clone(),
                    label: label.clone(),
                    reference_to: field.reference_to.clone(),
                    selected_targets: selected_targets.clone(),
                    selected_target_key_prefixes,
                    omitted_targets: omitted_targets.clone(),
                });

                if candidates.is_empty() {
                    omissions.push(SchemaOmission {
                        object: object.clone(),
                        field: field.name.clone(),
                        reason: "noSelectedReferenceTargets".to_string(),
                    });
                } else {
                    lookup_plans.push(LookupPlan {
                        field: field.name.clone(),
                        label,
                        candidates,
                    });
                }
            }

            selected_fields.sort();
            selected_fields.dedup();
            field_schemas.sort_by(|left, right| left.name.cmp(&right.name));
            lookup_schemas.sort_by(|left, right| left.field.cmp(&right.field));
            omissions.sort_by(|left, right| {
                left.field
                    .cmp(&right.field)
                    .then_with(|| left.reason.cmp(&right.reason))
            });

            let plan = ObjectPlan::from_describe(describe.clone(), &selected_fields)?;
            let schema_object = ObjectSchema {
                object: object.clone(),
                queryable: describe.queryable,
                key_prefix: describe.key_prefix.clone(),
                fields: field_schemas,
                lookups: lookup_schemas,
                omissions,
            };

            let plan_index = plans.len();
            plans.push(plan);
            plans_by_object.insert(
                object.clone(),
                CatalogPlan {
                    plan_index,
                    lookups: lookup_plans,
                    schema_revision: String::new(),
                },
            );
            schema_objects.push(schema_object);
        }

        schema_objects.sort_by(|left, right| left.object.cmp(&right.object));
        let mut schema = DiscoveredSchema {
            fingerprint: String::new(),
            objects: schema_objects,
        };
        let fingerprint = schema_fingerprint(&schema)?;
        schema.fingerprint = fingerprint.clone();
        for plan in plans_by_object.values_mut() {
            plan.schema_revision = fingerprint.clone();
        }

        Ok(Catalog {
            plans,
            schema,
            plans_by_object,
        })
    }

    pub fn record(&self, object: &str, record: Record) -> Result<GraphRecord> {
        let catalog_plan = self
            .plans_by_object
            .get(object)
            .with_context(|| format!("object '{object}' is not part of the active catalog"))?;
        let object_plan = self
            .plans
            .get(catalog_plan.plan_index)
            .with_context(|| format!("catalog plan index missing for object '{object}'"))?;
        let canonical_id = object_plan
            .canonical_record_id(&record.id)
            .with_context(|| {
                format!(
                    "record id '{}' does not belong to Salesforce object '{}'",
                    record.id, object
                )
            })?;

        let mut relationships = Vec::new();
        for lookup in &catalog_plan.lookups {
            let Some(value) = record.properties.get(&lookup.field) else {
                continue;
            };
            if matches!(value, ElementValue::Null) {
                continue;
            }
            let raw_target_id = match value {
                ElementValue::String(value) => value.as_ref(),
                other => {
                    bail!(
                        "Salesforce lookup field '{}' on '{}' must be string or null, found {:?}",
                        lookup.field,
                        object,
                        other
                    )
                }
            };
            let canonical_target_id = canonical_record_id(raw_target_id)?;
            let target_prefix = &canonical_target_id[..3];
            let mut matches = lookup
                .candidates
                .iter()
                .filter(|candidate| candidate.key_prefix == target_prefix);

            match (matches.next(), matches.next()) {
                (Some(target), None) => relationships.push(LookupRelationship {
                    field: lookup.field.clone(),
                    label: lookup.label.clone(),
                    target_object: target.object.clone(),
                    target_id: canonical_target_id.clone(),
                }),
                (None, _) => {}
                (Some(first), Some(second)) => {
                    bail!(
                        "Salesforce lookup '{}' on '{}' has ambiguous target keyPrefix '{}' ({} vs {})",
                        lookup.field,
                        object,
                        target_prefix,
                        first.object,
                        second.object
                    );
                }
            }
        }

        relationships.sort_by(|left, right| left.field.cmp(&right.field));
        let graph = GraphRecord {
            object: object_plan.name.clone(),
            id: canonical_id,
            properties: record.properties,
            relationships,
            observed_at_ms: record.timestamp,
            schema_revision: catalog_plan.schema_revision.clone(),
        };
        validate_graph_record(&graph)?;
        Ok(graph)
    }
}

fn string_property(name: &str) -> PropertySchema {
    PropertySchema {
        name: name.to_string(),
        data_type: Some(PropertyType::String),
        description: None,
    }
}

fn unique_label(labels: &BTreeSet<&str>) -> Option<String> {
    if labels.len() == 1 {
        labels.first().map(|label| (*label).to_string())
    } else {
        None
    }
}

impl GraphRecord {
    pub fn validate(&self) -> Result<()> {
        validate_graph_record(self)
    }

    pub fn key(&self) -> String {
        format!("{}:{}", self.object, self.id)
    }

    pub fn node(&self, source_id: &str, org_id: &str) -> Result<Element> {
        ensure!(!source_id.is_empty(), "source_id must not be empty");
        validate_identifier(&self.object)?;
        validate_org_id(org_id)?;
        let canonical_id = canonical_record_id(&self.id)?;
        let element_id = graph_element_id(org_id, &self.object, &canonical_id);

        let mut properties = ElementPropertyMap::new();
        properties.insert("Id", ElementValue::String(canonical_id.into()));
        for (name, value) in &self.properties {
            properties.insert(name.as_str(), value.clone());
        }

        Ok(Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(source_id, &element_id),
                labels: Arc::from(vec![Arc::from(self.object.as_str())]),
                effective_from: self.observed_at_ms,
            },
            properties,
        })
    }

    pub fn relations(&self, source_id: &str, org_id: &str) -> Result<Vec<Element>> {
        ensure!(!source_id.is_empty(), "source_id must not be empty");
        validate_identifier(&self.object)?;
        validate_org_id(org_id)?;
        let owner_id = canonical_record_id(&self.id)?;
        let owner_element_id = graph_element_id(org_id, &self.object, &owner_id);

        let mut relations = Vec::with_capacity(self.relationships.len());
        for relationship in &self.relationships {
            validate_identifier(&relationship.field)?;
            validate_identifier(&relationship.label)?;
            validate_identifier(&relationship.target_object)?;
            let target_id = canonical_record_id(&relationship.target_id)?;

            let relation_id =
                relation_element_id(org_id, &self.object, &owner_id, &relationship.field);
            let target_element_id =
                graph_element_id(org_id, &relationship.target_object, &target_id);

            let mut properties = ElementPropertyMap::new();
            properties.insert(
                "Field",
                ElementValue::String(relationship.field.clone().into()),
            );
            properties.insert(
                "TargetObject",
                ElementValue::String(relationship.target_object.clone().into()),
            );
            properties.insert("TargetId", ElementValue::String(target_id.clone().into()));

            relations.push(Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new(source_id, &relation_id),
                    labels: Arc::from(vec![Arc::from(relationship.label.as_str())]),
                    effective_from: self.observed_at_ms,
                },
                properties,
                in_node: ElementReference::new(source_id, &owner_element_id),
                out_node: ElementReference::new(source_id, &target_element_id),
            });
        }
        Ok(relations)
    }
}

pub fn graph_changes(
    source_id: &str,
    org_id: &str,
    previous: Option<&GraphRecord>,
    next: Option<&GraphRecord>,
    deleted_at_ms: u64,
) -> Result<Vec<SourceChange>> {
    ensure!(!source_id.is_empty(), "source_id must not be empty");
    validate_org_id(org_id)?;

    if let Some(previous) = previous {
        validate_graph_record(previous)?;
    }
    if let Some(next) = next {
        validate_graph_record(next)?;
    }

    match (previous, next) {
        (None, None) => bail!("graph_changes requires either previous or next record"),
        (None, Some(next)) => {
            let mut changes = Vec::new();
            changes.push(SourceChange::Insert {
                element: next.node(source_id, org_id)?,
            });
            for relation in next.relations(source_id, org_id)? {
                changes.push(SourceChange::Insert { element: relation });
            }
            Ok(changes)
        }
        (Some(previous), None) => {
            let mut changes = Vec::new();
            for relation in previous.relations(source_id, org_id)? {
                let Element::Relation { metadata, .. } = relation else {
                    bail!("expected relationship element for Salesforce relation delete")
                };
                changes.push(SourceChange::Delete {
                    metadata: metadata.clone_with_effective_from(deleted_at_ms),
                });
            }
            changes.push(SourceChange::Delete {
                metadata: node_metadata(
                    source_id,
                    org_id,
                    &previous.object,
                    &previous.id,
                    deleted_at_ms,
                )?,
            });
            Ok(changes)
        }
        (Some(previous), Some(next)) => {
            ensure!(
                previous.object == next.object && previous.id == next.id,
                "previous and next graph records must refer to the same Salesforce record"
            );

            let previous_relations = relationship_by_field(previous)?;
            let next_relations = relationship_by_field(next)?;

            let mut relation_deletes = Vec::new();
            for (field, previous_relation) in &previous_relations {
                if next_relations
                    .get(field)
                    .is_none_or(|next_relation| next_relation != previous_relation)
                {
                    relation_deletes.push((*previous_relation).clone());
                }
            }
            relation_deletes.sort_by(|left, right| left.field.cmp(&right.field));

            let mut relation_inserts = Vec::new();
            for (field, next_relation) in &next_relations {
                if previous_relations
                    .get(field)
                    .is_none_or(|previous_relation| previous_relation != next_relation)
                {
                    relation_inserts.push((*next_relation).clone());
                }
            }
            relation_inserts.sort_by(|left, right| left.field.cmp(&right.field));

            let mut relation_updates = Vec::new();
            if previous.observed_at_ms != next.observed_at_ms {
                for (field, next_relation) in &next_relations {
                    if previous_relations
                        .get(field)
                        .is_some_and(|previous_relation| *previous_relation == *next_relation)
                    {
                        relation_updates.push((*next_relation).clone());
                    }
                }
            }
            relation_updates.sort_by(|left, right| left.field.cmp(&right.field));

            let patch = property_patch(&previous.properties, &next.properties);
            if relation_deletes.is_empty()
                && relation_inserts.is_empty()
                && relation_updates.is_empty()
                && patch.is_empty()
            {
                return Ok(Vec::new());
            }

            let mut changes = Vec::new();
            for relationship in relation_deletes {
                changes.push(SourceChange::Delete {
                    metadata: relation_metadata(
                        source_id,
                        org_id,
                        &next.object,
                        &next.id,
                        &relationship,
                        next.observed_at_ms,
                    )?,
                });
            }

            if !patch.is_empty() {
                changes.push(SourceChange::Update {
                    element: node_from_patch(source_id, org_id, next, patch)?,
                });
            }

            for relationship in relation_updates {
                changes.push(SourceChange::Update {
                    element: relation_element(source_id, org_id, next, &relationship)?,
                });
            }

            for relationship in relation_inserts {
                changes.push(SourceChange::Insert {
                    element: relation_element(source_id, org_id, next, &relationship)?,
                });
            }

            Ok(changes)
        }
    }
}

fn validate_graph_record(record: &GraphRecord) -> Result<()> {
    validate_identifier(&record.object)?;
    canonical_record_id(&record.id)
        .with_context(|| format!("invalid Salesforce record id '{}'", record.id))?;

    let mut seen_fields = BTreeSet::new();
    for relationship in &record.relationships {
        validate_identifier(&relationship.field)?;
        validate_identifier(&relationship.label)?;
        validate_identifier(&relationship.target_object)?;
        canonical_record_id(&relationship.target_id)?;
        ensure!(
            seen_fields.insert(relationship.field.clone()),
            "graph record '{}' contains duplicate relationship field '{}'",
            record.key(),
            relationship.field
        );
    }
    Ok(())
}

fn relationship_by_field(record: &GraphRecord) -> Result<HashMap<String, &LookupRelationship>> {
    let mut by_field = HashMap::new();
    for relationship in &record.relationships {
        ensure!(
            by_field
                .insert(relationship.field.clone(), relationship)
                .is_none(),
            "graph record '{}' has duplicate relationship field '{}'",
            record.key(),
            relationship.field
        );
    }
    Ok(by_field)
}

fn property_patch(previous: &Properties, next: &Properties) -> Properties {
    let mut keys = BTreeSet::new();
    for key in previous.keys() {
        keys.insert(key.clone());
    }
    for key in next.keys() {
        keys.insert(key.clone());
    }

    let mut patch = Properties::new();
    for key in keys {
        match (previous.get(&key), next.get(&key)) {
            (Some(previous), Some(next)) if previous == next => {}
            (_, Some(next)) => {
                patch.insert(key, next.clone());
            }
            (Some(_), None) => {
                patch.insert(key, ElementValue::Null);
            }
            (None, None) => {}
        }
    }
    patch
}

fn node_from_patch(
    source_id: &str,
    org_id: &str,
    record: &GraphRecord,
    patch: Properties,
) -> Result<Element> {
    let canonical_id = canonical_record_id(&record.id)?;
    let element_id = graph_element_id(org_id, &record.object, &canonical_id);
    let mut properties = ElementPropertyMap::new();
    properties.insert("Id", ElementValue::String(canonical_id.into()));
    for (name, value) in patch {
        properties.insert(name.as_str(), value);
    }

    Ok(Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new(source_id, &element_id),
            labels: Arc::from(vec![Arc::from(record.object.as_str())]),
            effective_from: record.observed_at_ms,
        },
        properties,
    })
}

fn relation_element(
    source_id: &str,
    org_id: &str,
    owner: &GraphRecord,
    relationship: &LookupRelationship,
) -> Result<Element> {
    let owner_id = canonical_record_id(&owner.id)?;
    let target_id = canonical_record_id(&relationship.target_id)?;
    let owner_element_id = graph_element_id(org_id, &owner.object, &owner_id);
    let target_element_id = graph_element_id(org_id, &relationship.target_object, &target_id);
    let relation_id = relation_element_id(org_id, &owner.object, &owner_id, &relationship.field);

    let mut properties = ElementPropertyMap::new();
    properties.insert(
        "Field",
        ElementValue::String(relationship.field.clone().into()),
    );
    properties.insert(
        "TargetObject",
        ElementValue::String(relationship.target_object.clone().into()),
    );
    properties.insert("TargetId", ElementValue::String(target_id.into()));

    Ok(Element::Relation {
        metadata: ElementMetadata {
            reference: ElementReference::new(source_id, &relation_id),
            labels: Arc::from(vec![Arc::from(relationship.label.as_str())]),
            effective_from: owner.observed_at_ms,
        },
        properties,
        in_node: ElementReference::new(source_id, &owner_element_id),
        out_node: ElementReference::new(source_id, &target_element_id),
    })
}

fn node_metadata(
    source_id: &str,
    org_id: &str,
    object: &str,
    id: &str,
    effective_from: u64,
) -> Result<ElementMetadata> {
    let canonical_id = canonical_record_id(id)?;
    Ok(ElementMetadata {
        reference: ElementReference::new(
            source_id,
            &graph_element_id(org_id, object, &canonical_id),
        ),
        labels: Arc::from(vec![Arc::from(object)]),
        effective_from,
    })
}

fn relation_metadata(
    source_id: &str,
    org_id: &str,
    owner_object: &str,
    owner_id: &str,
    relationship: &LookupRelationship,
    effective_from: u64,
) -> Result<ElementMetadata> {
    let canonical_owner_id = canonical_record_id(owner_id)?;
    Ok(ElementMetadata {
        reference: ElementReference::new(
            source_id,
            &relation_element_id(
                org_id,
                owner_object,
                &canonical_owner_id,
                &relationship.field,
            ),
        ),
        labels: Arc::from(vec![Arc::from(relationship.label.as_str())]),
        effective_from,
    })
}

fn relation_element_id(org_id: &str, object: &str, owner_id: &str, field: &str) -> String {
    format!("sfrel:{org_id}:{object}:{owner_id}:{field}")
}

fn schema_fingerprint(schema: &DiscoveredSchema) -> Result<String> {
    let mut normalized = schema.clone();
    normalized.fingerprint.clear();
    let canonical =
        serde_json::to_vec(&normalized).context("failed to serialize schema fingerprint input")?;
    Ok(format!("{:x}", Sha256::digest(canonical)))
}

fn kind_name(kind: &Kind) -> &'static str {
    match kind {
        Kind::Text => "text",
        Kind::Bool => "bool",
        Kind::Integer => "integer",
        Kind::Float => "float",
        Kind::Date => "date",
        Kind::Time => "time",
        Kind::Instant => "instant",
        Kind::MultiSelect => "multiSelect",
        Kind::Compound(_) => "compound",
    }
}

trait CloneWithEffectiveFrom {
    fn clone_with_effective_from(&self, effective_from: u64) -> Self;
}

impl CloneWithEffectiveFrom for ElementMetadata {
    fn clone_with_effective_from(&self, effective_from: u64) -> Self {
        ElementMetadata {
            reference: self.reference.clone(),
            labels: self.labels.clone(),
            effective_from,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account_describe() -> Describe {
        Describe {
            name: "Account".to_string(),
            queryable: true,
            key_prefix: "001".to_string(),
            fields: vec![
                describe_field("Id", "id", false),
                describe_field("Name", "string", true).with_compound("Name"),
                describe_field("ParentId", "reference", true)
                    .with_relationship("Parent")
                    .with_reference_to(vec!["Account"]),
                describe_field("BillingAddress", "address", true),
                describe_field("BillingStreet", "string", true).with_compound("BillingAddress"),
                describe_field("LastModifiedDate", "datetime", false),
            ],
        }
    }

    fn contact_describe() -> Describe {
        Describe {
            name: "Contact".to_string(),
            queryable: true,
            key_prefix: "003".to_string(),
            fields: vec![
                describe_field("Id", "id", false),
                describe_field("Name", "string", true).with_compound("Name"),
                describe_field("AccountId", "reference", true)
                    .with_relationship("Account")
                    .with_reference_to(vec!["Account"]),
                describe_field("ReportsToId", "reference", true)
                    .with_relationship("ReportsTo")
                    .with_reference_to(vec!["Contact"]),
                describe_field("WhatId", "reference", true)
                    .with_relationship("What")
                    .with_reference_to(vec!["Opportunity", "Case"]),
                describe_field("Description", "textarea", true),
                describe_field("MailingAddress", "address", true),
                describe_field("MailingStreet", "string", true).with_compound("MailingAddress"),
                describe_field("LastModifiedDate", "datetime", false),
            ],
        }
    }

    #[test]
    fn source_schema_matches_projected_values_and_omits_unsupported_fields() -> Result<()> {
        let mut hidden = describe_field("Hidden__c", "string", true);
        hidden.deprecated_and_hidden = true;
        let mut unqueryable = describe_field("Unavailable__c", "string", true);
        unqueryable.queryable = false;
        let catalog = Catalog::from_describes(vec![Describe {
            name: "Widget__c".into(),
            queryable: true,
            key_prefix: "a01".into(),
            fields: vec![
                describe_field("Id", "id", false),
                describe_field("Name", "string", false),
                describe_field("Active__c", "boolean", false),
                describe_field("Count__c", "int", false),
                describe_field("Amount__c", "currency", false),
                describe_field("Day__c", "date", false),
                describe_field("Time__c", "time", false),
                describe_field("LastModifiedDate", "datetime", false),
                describe_field("Choices__c", "multipicklist", false),
                describe_field("Address__c", "address", true),
                describe_field("Address__City__s", "string", true).with_compound("Address__c"),
                describe_field("Binary__c", "base64", true),
                describe_field("Future__c", "unknown", true),
                hidden,
                unqueryable,
            ],
        }])?;
        let record = catalog.plans[0].snapshot(&serde_json::json!({
            "Id": "a01000000000001",
            "Name": "Example widget",
            "Active__c": true,
            "Count__c": 42,
            "Amount__c": 12.5,
            "Day__c": "2026-10-23",
            "Time__c": "13:00:00.125Z",
            "LastModifiedDate": "2026-10-23T13:00:00.000Z",
            "Choices__c": "B;A",
            "Address__City__s": "Example city"
        }))?;
        let graph = catalog.record("Widget__c", record)?;
        let Element::Node { properties, .. } = graph.node("sf", "00D000000000001AAA")? else {
            panic!("expected node");
        };
        let schema = catalog.source_schema();
        let node = &schema.nodes[0];
        assert_eq!(node.label, "Widget__c");
        let expected = [
            ("Id", PropertyType::String),
            ("Name", PropertyType::String),
            ("Active__c", PropertyType::Boolean),
            ("Count__c", PropertyType::Integer),
            ("Amount__c", PropertyType::Float),
            ("Day__c", PropertyType::String),
            ("Time__c", PropertyType::String),
            ("LastModifiedDate", PropertyType::Timestamp),
            ("Choices__c", PropertyType::Json),
            ("Address__City__s", PropertyType::String),
        ];
        assert_eq!(node.properties.len(), expected.len());
        for (name, expected_type) in expected {
            let property = node.properties.iter().find(|p| p.name == name).unwrap();
            assert_eq!(property.data_type, Some(expected_type), "{name}");
            let value = properties.get(name).unwrap();
            assert!(
                matches!(
                    (expected_type, value),
                    (PropertyType::String, ElementValue::String(_))
                        | (PropertyType::Boolean, ElementValue::Bool(_))
                        | (PropertyType::Integer, ElementValue::Integer(_))
                        | (PropertyType::Float, ElementValue::Float(_))
                        | (PropertyType::Timestamp, ElementValue::ZonedDateTime(_))
                        | (PropertyType::Json, ElementValue::List(_))
                ),
                "{name}: {value:?}"
            );
            if matches!(name, "Day__c" | "Time__c" | "Choices__c") {
                assert!(property.description.is_some(), "{name}");
            }
        }
        assert!(node.properties.windows(2).all(|p| p[0].name < p[1].name));
        Ok(())
    }

    #[test]
    fn source_schema_groups_relationship_labels_without_inventing_endpoints() -> Result<()> {
        let mut account = account_describe();
        account.fields.push(
            describe_field("OwnerId", "reference", true)
                .with_relationship("Shared")
                .with_reference_to(vec!["Account"]),
        );
        let mut contact = contact_describe();
        contact.fields.extend([
            describe_field("OwnerId", "reference", true)
                .with_relationship("Shared")
                .with_reference_to(vec!["Account"]),
            describe_field("RelatedId", "reference", true)
                .with_relationship("Related")
                .with_reference_to(vec!["Contact", "Account", "Opportunity"]),
            describe_field("SecondaryAccountId", "reference", true)
                .with_relationship("Account")
                .with_reference_to(vec!["Account"]),
        ]);
        let mut describes = vec![contact, account];
        let catalog = Catalog::from_describes(describes.clone())?;
        let schema = catalog.source_schema();
        let relation = |label| schema.relations.iter().find(|r| r.label == label).unwrap();
        assert_eq!(relation("Account").from.as_deref(), Some("Contact"));
        assert_eq!(relation("Account").to.as_deref(), Some("Account"));
        assert_eq!(relation("Shared").from, None);
        assert_eq!(relation("Shared").to.as_deref(), Some("Account"));
        assert_eq!(relation("Related").from.as_deref(), Some("Contact"));
        assert_eq!(relation("Related").to, None);
        assert!(!schema.relations.iter().any(|r| r.label == "What"));
        assert_eq!(
            schema
                .relations
                .iter()
                .filter(|r| r.label == "Account")
                .count(),
            1
        );
        let mut merged = drasi_lib::schema::GraphSchema::default();
        merged.merge_source_schema("sf", &schema);
        assert_eq!(merged.relations["Related"].to, None);
        assert_eq!(merged.relations["Shared"].from, None);

        let record = catalog.record(
            "Contact",
            Record {
                id: "003000000000001AAA".into(),
                properties: BTreeMap::from([(
                    "AccountId".into(),
                    ElementValue::String("001000000000001AAA".into()),
                )]),
                timestamp: 1,
            },
        )?;
        let edges = record.relations("sf", "00D000000000001AAA")?;
        let Element::Relation { properties, .. } = &edges[0] else {
            panic!("expected relation");
        };
        assert_eq!(relation("Account").properties.len(), 3);
        for property in &relation("Account").properties {
            assert_eq!(property.data_type, Some(PropertyType::String));
            assert!(matches!(
                properties.get(&property.name),
                Some(ElementValue::String(_))
            ));
        }

        describes.reverse();
        for describe in &mut describes {
            describe.fields.reverse();
            for field in &mut describe.fields {
                field.reference_to.reverse();
            }
        }
        assert_eq!(Catalog::from_describes(describes)?.source_schema(), schema);
        Ok(())
    }

    #[test]
    fn source_schema_always_declares_id_once() -> Result<()> {
        for include_id in [true, false] {
            let mut describe = account_describe();
            if !include_id {
                describe.fields.retain(|field| field.name != "Id");
            }
            let schema = Catalog::from_describes(vec![describe])?.source_schema();
            let ids = schema.nodes[0]
                .properties
                .iter()
                .filter(|property| property.name == "Id")
                .collect::<Vec<_>>();
            assert_eq!(ids.len(), 1);
            assert_eq!(ids[0].data_type, Some(PropertyType::String));
        }
        Ok(())
    }

    #[test]
    fn catalog_builds_schema_and_resolves_contact_account_lookup() -> Result<()> {
        let catalog = Catalog::from_describes(vec![contact_describe(), account_describe()])?;

        assert_eq!(catalog.plans.len(), 2);
        assert_eq!(catalog.schema.objects.len(), 2);
        assert!(!catalog.schema.fingerprint.is_empty());

        let contact = catalog.record(
            "Contact",
            Record {
                id: "003000000000001AAA".to_string(),
                properties: BTreeMap::from([
                    ("Name".to_string(), ElementValue::String("Ada".into())),
                    (
                        "AccountId".to_string(),
                        ElementValue::String("001000000000001AAA".into()),
                    ),
                    (
                        "Description".to_string(),
                        ElementValue::String("test".into()),
                    ),
                ]),
                timestamp: 1_700_000_000_000,
            },
        )?;

        assert_eq!(contact.object, "Contact");
        assert_eq!(contact.relationships.len(), 1);
        assert_eq!(contact.relationships[0].field, "AccountId");
        assert_eq!(contact.relationships[0].label, "Account");
        assert_eq!(contact.relationships[0].target_object, "Account");
        assert_eq!(contact.relationships[0].target_id, "001000000000001AAA");

        let contact_schema = catalog
            .schema
            .objects
            .iter()
            .find(|object| object.object == "Contact")
            .expect("contact schema");
        assert!(contact_schema
            .omissions
            .iter()
            .any(|omission| omission.field == "WhatId"
                && omission.reason == "noSelectedReferenceTargets"));
        assert!(contact_schema
            .omissions
            .iter()
            .any(|omission| omission.field == "MailingAddress"
                && omission.reason == "unsupportedCompoundParentType"));
        Ok(())
    }

    #[test]
    fn catalog_rejects_ambiguous_polymorphic_key_prefixes() {
        let mut contact = contact_describe();
        contact.fields.push(
            describe_field("OwnerOrAccount", "reference", true)
                .with_relationship("OwnerOrAccount")
                .with_reference_to(vec!["Account", "AccountReplica"]),
        );
        let replica = Describe {
            name: "AccountReplica".to_string(),
            queryable: true,
            key_prefix: "001".to_string(),
            fields: vec![describe_field("Id", "id", false)],
        };
        let error = Catalog::from_describes(vec![account_describe(), contact, replica]).expect_err(
            "duplicated target keyPrefix in polymorphic selected targets must be rejected",
        );
        assert!(
            format!("{error:#}").contains("share keyPrefix"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn graph_changes_remove_old_relations_then_update_then_insert_new_relations() -> Result<()> {
        let previous = GraphRecord {
            object: "Contact".to_string(),
            id: "003000000000001AAA".to_string(),
            properties: BTreeMap::from([
                ("Name".to_string(), ElementValue::String("Ada".into())),
                ("Phone".to_string(), ElementValue::String("555-0100".into())),
            ]),
            relationships: vec![LookupRelationship {
                field: "AccountId".to_string(),
                label: "Account".to_string(),
                target_object: "Account".to_string(),
                target_id: "001000000000001AAA".to_string(),
            }],
            observed_at_ms: 10,
            schema_revision: "rev-1".to_string(),
        };
        let next = GraphRecord {
            object: "Contact".to_string(),
            id: "003000000000001AAA".to_string(),
            properties: BTreeMap::from([("Name".to_string(), ElementValue::String("Ada".into()))]),
            relationships: vec![LookupRelationship {
                field: "AccountId".to_string(),
                label: "Account".to_string(),
                target_object: "Account".to_string(),
                target_id: "001000000000002AAA".to_string(),
            }],
            observed_at_ms: 20,
            schema_revision: "rev-1".to_string(),
        };

        let changes = graph_changes(
            "salesforce-source",
            "00D000000000001AAA",
            Some(&previous),
            Some(&next),
            99,
        )?;
        assert_eq!(changes.len(), 3);
        assert!(matches!(changes[0], SourceChange::Delete { .. }));
        assert!(matches!(changes[1], SourceChange::Update { .. }));
        assert!(matches!(changes[2], SourceChange::Insert { .. }));

        let SourceChange::Update { ref element } = changes[1] else {
            panic!("expected update change");
        };
        let Element::Node { properties, .. } = element else {
            panic!("expected node update");
        };
        assert_eq!(
            properties.get("Id"),
            Some(&ElementValue::String("003000000000001AAA".into())),
            "update payload must include canonical Id for stable node identity"
        );
        assert_eq!(
            properties.get("Phone"),
            Some(&ElementValue::Null),
            "removed sparse property must be explicitly nulled"
        );
        let SourceChange::Delete { metadata } = &changes[0] else {
            panic!("expected relation delete before update");
        };
        assert!(
            metadata
                .labels
                .iter()
                .any(|label| label.as_ref() == "Account"),
            "relation delete must preserve previous relationship label"
        );
        let SourceChange::Insert { element } = &changes[2] else {
            panic!("expected relation insert after update");
        };
        let Element::Relation { metadata, .. } = element else {
            panic!("expected relation insert element");
        };
        assert!(
            metadata
                .labels
                .iter()
                .any(|label| label.as_ref() == "Account"),
            "relation insert must preserve next relationship label"
        );
        Ok(())
    }

    #[test]
    fn graph_changes_rejects_identity_mismatches() {
        let previous = GraphRecord {
            object: "Account".to_string(),
            id: "001000000000001AAA".to_string(),
            properties: BTreeMap::new(),
            relationships: Vec::new(),
            observed_at_ms: 1,
            schema_revision: "rev-1".to_string(),
        };
        let next = GraphRecord {
            object: "Contact".to_string(),
            id: "003000000000001AAA".to_string(),
            properties: BTreeMap::new(),
            relationships: Vec::new(),
            observed_at_ms: 2,
            schema_revision: "rev-1".to_string(),
        };
        let error = graph_changes(
            "salesforce-source",
            "00D000000000001AAA",
            Some(&previous),
            Some(&next),
            3,
        )
        .expect_err("mismatching record identities must be rejected");
        assert!(
            format!("{error:#}").contains("same Salesforce record"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn graph_changes_updates_retained_relations_when_owner_timestamp_changes() -> Result<()> {
        let previous = GraphRecord {
            object: "Contact".to_string(),
            id: "003000000000001AAA".to_string(),
            properties: BTreeMap::from([
                ("Name".to_string(), ElementValue::String("Ada".into())),
                ("Title".to_string(), ElementValue::String("Engineer".into())),
            ]),
            relationships: vec![LookupRelationship {
                field: "AccountId".to_string(),
                label: "Account".to_string(),
                target_object: "Account".to_string(),
                target_id: "001000000000001AAA".to_string(),
            }],
            observed_at_ms: 10,
            schema_revision: "rev-1".to_string(),
        };

        let next = GraphRecord {
            object: "Contact".to_string(),
            id: "003000000000001AAA".to_string(),
            properties: BTreeMap::from([
                ("Name".to_string(), ElementValue::String("Ada".into())),
                (
                    "Title".to_string(),
                    ElementValue::String("Principal Engineer".into()),
                ),
            ]),
            relationships: vec![LookupRelationship {
                field: "AccountId".to_string(),
                label: "Account".to_string(),
                target_object: "Account".to_string(),
                target_id: "001000000000001AAA".to_string(),
            }],
            observed_at_ms: 20,
            schema_revision: "rev-1".to_string(),
        };

        let changes = graph_changes(
            "salesforce-source",
            "00D000000000001AAA",
            Some(&previous),
            Some(&next),
            99,
        )?;
        assert_eq!(changes.len(), 2);
        assert!(matches!(changes[0], SourceChange::Update { .. }));
        assert!(matches!(changes[1], SourceChange::Update { .. }));

        let SourceChange::Update { element } = &changes[1] else {
            panic!("expected retained relation update");
        };
        let Element::Relation { metadata, .. } = element else {
            panic!("expected relation update element");
        };
        assert_eq!(
            metadata.effective_from, 20,
            "retained relation effective_from must track owner observation time"
        );
        Ok(())
    }

    #[test]
    fn graph_changes_owner_property_update_matches_snapshot_relation_timestamps() -> Result<()> {
        let previous = GraphRecord {
            object: "Contact".to_string(),
            id: "003000000000001AAA".to_string(),
            properties: BTreeMap::from([("Name".to_string(), ElementValue::String("Ada".into()))]),
            relationships: vec![LookupRelationship {
                field: "AccountId".to_string(),
                label: "Account".to_string(),
                target_object: "Account".to_string(),
                target_id: "001000000000001AAA".to_string(),
            }],
            observed_at_ms: 10,
            schema_revision: "rev-1".to_string(),
        };
        let next = GraphRecord {
            object: "Contact".to_string(),
            id: "003000000000001AAA".to_string(),
            properties: BTreeMap::from([(
                "Name".to_string(),
                ElementValue::String("Ada Lovelace".into()),
            )]),
            relationships: vec![LookupRelationship {
                field: "AccountId".to_string(),
                label: "Account".to_string(),
                target_object: "Account".to_string(),
                target_id: "001000000000001AAA".to_string(),
            }],
            observed_at_ms: 20,
            schema_revision: "rev-1".to_string(),
        };

        let mut journal_state =
            graph_state_from_record("salesforce-source", "00D000000000001AAA", &previous)?;
        let changes = graph_changes(
            "salesforce-source",
            "00D000000000001AAA",
            Some(&previous),
            Some(&next),
            99,
        )?;
        apply_graph_changes(&mut journal_state, &changes);

        let snapshot_state =
            graph_state_from_record("salesforce-source", "00D000000000001AAA", &next)?;
        assert_eq!(
            journal_state, snapshot_state,
            "journal deltas and snapshot reconstruction must converge including effective_from"
        );
        Ok(())
    }

    fn graph_state_from_record(
        source_id: &str,
        org_id: &str,
        record: &GraphRecord,
    ) -> Result<BTreeMap<String, Element>> {
        let mut state = BTreeMap::new();
        let node = record.node(source_id, org_id)?;
        state.insert(node.get_reference().to_string(), node);
        for relation in record.relations(source_id, org_id)? {
            state.insert(relation.get_reference().to_string(), relation);
        }
        Ok(state)
    }

    fn apply_graph_changes(state: &mut BTreeMap<String, Element>, changes: &[SourceChange]) {
        for change in changes {
            match change {
                SourceChange::Insert { element } | SourceChange::Update { element } => {
                    state.insert(element.get_reference().to_string(), element.clone());
                }
                SourceChange::Delete { metadata } => {
                    state.remove(&metadata.reference.to_string());
                }
                SourceChange::Future { .. } => {
                    panic!("unexpected Future source change in graph delta regression");
                }
            }
        }
    }

    trait DescribeFieldExt {
        fn with_relationship(self, label: &str) -> Self;
        fn with_reference_to(self, targets: Vec<&str>) -> Self;
        fn with_compound(self, compound: &str) -> Self;
    }

    impl DescribeFieldExt for crate::projection::DescribeField {
        fn with_relationship(mut self, label: &str) -> Self {
            self.relationship_name = Some(label.to_string());
            self
        }

        fn with_reference_to(mut self, targets: Vec<&str>) -> Self {
            self.reference_to = targets.into_iter().map(str::to_string).collect();
            self
        }

        fn with_compound(mut self, compound: &str) -> Self {
            self.compound_field_name = Some(compound.to_string());
            self
        }
    }

    fn describe_field(
        name: &str,
        field_type: &str,
        nillable: bool,
    ) -> crate::projection::DescribeField {
        crate::projection::DescribeField {
            name: name.to_string(),
            field_type: field_type.to_string(),
            queryable: true,
            nillable,
            relationship_name: None,
            reference_to: Vec::new(),
            deprecated_and_hidden: false,
            compound_field_name: None,
        }
    }
}
