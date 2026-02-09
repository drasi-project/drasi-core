use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use drasi_core::models::{Element, ElementMetadata, ElementReference, ElementValue, SourceChange};
use mongodb::bson::{Bson, Document};
use std::collections::BTreeMap;
use std::sync::Arc;
use ordered_float::OrderedFloat;

pub fn bson_to_element_value(bson: Bson) -> Result<ElementValue> {
    match bson {
        Bson::Double(v) => Ok(ElementValue::Float(ordered_float::OrderedFloat(v))),
        Bson::String(v) => Ok(ElementValue::String(v.into())),
        Bson::Array(v) => {
            let mut list = Vec::new();
            for item in v {
                list.push(bson_to_element_value(item)?);
            }
            Ok(ElementValue::List(list))
        }
        Bson::Document(v) => {
            let mut map = BTreeMap::new();
            for (key, value) in v {
                map.insert(key, bson_to_element_value(value)?);
            }
            Ok(ElementValue::Object(map.into()))
        }
        Bson::Boolean(v) => Ok(ElementValue::Bool(v)),
        Bson::Null => Ok(ElementValue::Null),
        Bson::Int32(v) => Ok(ElementValue::Integer(v as i64)),
        Bson::Int64(v) => Ok(ElementValue::Integer(v)),
        Bson::ObjectId(oid) => Ok(ElementValue::String(oid.to_hex().into())),
        Bson::DateTime(dt) => {
            let millis = dt.timestamp_millis();
            let seconds = millis / 1000;
            let nsecs = (millis % 1000) * 1_000_000;
            if let Some(dt) = DateTime::from_timestamp(seconds, nsecs as u32) {
                Ok(ElementValue::String(dt.to_rfc3339().into()))
            } else {
                Ok(ElementValue::String(dt.to_string().into()))
            }
        }
        // For other types, convert to string representation
        other => Ok(ElementValue::String(other.to_string().into())),
    }
}

pub fn hydrate_dot_notation(doc: &Document, removed_fields: &[String]) -> Result<Document> {
    let mut hydrated = Document::new();

    // Process updated fields
    for (key, value) in doc {
        insert_dot_notation(&mut hydrated, key, value.clone())?;
    }

    // Process removed fields - set them to Null
    for field in removed_fields {
        insert_dot_notation(&mut hydrated, field, Bson::Null)?;
    }

    Ok(hydrated)
}

fn insert_dot_notation(doc: &mut Document, key: &str, value: Bson) -> Result<()> {
    if key.contains('.') {
        let parts: Vec<&str> = key.splitn(2, '.').collect();
        let head = parts[0];
        let tail = parts[1];

        let entry = doc.entry(head.to_string()).or_insert_with(|| Bson::Document(Document::new()));
        
        if let Bson::Document(sub_doc) = entry {
            insert_dot_notation(sub_doc, tail, value)?;
        } else {
            // Conflict: trying to treat a non-document value as a document
            // For MongoDB hydration, this might happen if structure changes.
            // We'll overwrite with a new document to proceed.
            let mut new_doc = Document::new();
            insert_dot_notation(&mut new_doc, tail, value)?;
            doc.insert(head.to_string(), Bson::Document(new_doc));
        }
    } else {
        doc.insert(key, value);
    }
    Ok(())
}

pub fn change_stream_event_to_source_change(
    event: mongodb::change_stream::event::ChangeStreamEvent<Document>,
    source_id: &str,
    collection_name: &str,
) -> Result<Option<SourceChange>> {
    let operation_type = event.operation_type;
    let document_key = event.document_key.as_ref().ok_or_else(|| anyhow!("Missing document_key"))?;
    
    // Extract _id for element ID
    let id_bson = document_key.get("_id").ok_or_else(|| anyhow!("Missing _id in document_key"))?;
    let id_str = match id_bson {
        Bson::ObjectId(oid) => oid.to_hex(),
        Bson::String(s) => s.clone(),
        Bson::Int32(i) => i.to_string(),
        Bson::Int64(i) => i.to_string(),
        other => other.to_string(), // Fallback for complex keys
    };
    
    let element_id = format!("{}:{}", collection_name, id_str);
    let labels = Arc::from([Arc::from(collection_name)]);
    
    // Determine effective timestamp
    // Use wall time if available, otherwise cluster time, otherwise current time
    let effective_from = if let Some(dt) = event.wall_time {
        dt.timestamp_millis() as u64
    } else if let Some(ts) = event.cluster_time {
         // Timestamp is (seconds, increment)
         // We'll just use seconds * 1000 to get millis
         (ts.time as u64) * 1000
    } else {
        Utc::now().timestamp_millis() as u64
    };

    let metadata = ElementMetadata {
        reference: ElementReference::new(source_id, &element_id),
        labels,
        effective_from,
    };

    match operation_type {
        mongodb::change_stream::event::OperationType::Insert => {
            let full_document = event.full_document.ok_or_else(|| anyhow!("Missing full_document for insert"))?;
            let properties = document_to_properties(&full_document)?;
            
            Ok(Some(SourceChange::Insert {
                element: Element::Node {
                    metadata,
                    properties,
                },
            }))
        }
        mongodb::change_stream::event::OperationType::Replace => {
            let full_document = event.full_document.ok_or_else(|| anyhow!("Missing full_document for replace"))?;
            let properties = document_to_properties(&full_document)?;
            
            // Replace is effectively an Update where we provide the full new state
            // Drasi treats it as Update because we are updating an existing element
            Ok(Some(SourceChange::Update {
                element: Element::Node {
                    metadata,
                    properties,
                },
            }))
        }
        mongodb::change_stream::event::OperationType::Update => {
            let update_desc = event.update_description.ok_or_else(|| anyhow!("Missing update_description"))?;
            let updated_fields = update_desc.updated_fields;
            let removed_fields = update_desc.removed_fields;
            
            // Hydrate dot notation and handle removed fields
            let hydrated_update = hydrate_dot_notation(&updated_fields, &removed_fields)?;
            let properties = document_to_properties(&hydrated_update)?;
            
            Ok(Some(SourceChange::Update {
                element: Element::Node {
                    metadata,
                    properties,
                },
            }))
        }
        mongodb::change_stream::event::OperationType::Delete => {
            Ok(Some(SourceChange::Delete { metadata }))
        }
        _ => {
            // Ignore other events (invalidate, drop, rename, etc. for now)
            Ok(None)
        }
    }
}

fn document_to_properties(doc: &Document) -> Result<drasi_core::models::ElementPropertyMap> {
    let mut properties = drasi_core::models::ElementPropertyMap::new();
    for (key, value) in doc {
        // Exclude _id from properties
        if key == "_id" {
            continue;
        }
        properties.insert(key, bson_to_element_value(value.clone())?);
    }
    Ok(properties)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mongodb::bson::{doc, Bson};
    use drasi_core::models::ElementValue;

    #[test]
    fn test_bson_to_element_value_simple() {
        assert_eq!(bson_to_element_value(Bson::String("hello".to_string())).unwrap(), ElementValue::String("hello".into()));
        assert_eq!(bson_to_element_value(Bson::Int32(42)).unwrap(), ElementValue::Integer(42));
        assert_eq!(bson_to_element_value(Bson::Double(3.14)).unwrap(), ElementValue::Float(ordered_float::OrderedFloat(3.14)));
        assert_eq!(bson_to_element_value(Bson::Boolean(true)).unwrap(), ElementValue::Bool(true));
        assert_eq!(bson_to_element_value(Bson::Null).unwrap(), ElementValue::Null);
    }

    #[test]
    fn test_bson_to_element_value_complex() {
        let doc = doc! {
            "key": "value",
            "list": [1, 2, 3],
            "nested": { "a": 1 }
        };
        let val = bson_to_element_value(Bson::Document(doc)).unwrap();
        if let ElementValue::Object(map) = val {
            assert_eq!(map.get("key").unwrap(), &ElementValue::String("value".into()));
            if let ElementValue::List(list) = map.get("list").unwrap() {
                assert_eq!(list.len(), 3);
            } else {
                panic!("Expected List");
            }
        } else {
             panic!("Expected Object");
        }
    }

    #[test]
    fn test_hydrate_dot_notation() {
        let updates = doc! {
            "a.b": 1,
            "a.c": 2,
            "x": 3
        };
        let removed = vec!["y".to_string(), "z.w".to_string()];
        
        let hydrated = hydrate_dot_notation(&updates, &removed).unwrap();
        
        println!("Hydrated: {:?}", hydrated);
        
        assert_eq!(hydrated.get_document("a").unwrap().get_i32("b").unwrap(), 1);
        assert_eq!(hydrated.get_document("a").unwrap().get_i32("c").unwrap(), 2);
        assert_eq!(hydrated.get_i32("x").unwrap(), 3);
        assert_eq!(hydrated.get("y").unwrap(), &Bson::Null);
        // Note: dot notation for removed fields is processed as: z: { w: Null }
        assert_eq!(hydrated.get_document("z").unwrap().get("w").unwrap(), &Bson::Null);
    }
}
