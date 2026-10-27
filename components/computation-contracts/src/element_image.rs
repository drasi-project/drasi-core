// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Canonical, lossless data image of a graph [`Element`].
//!
//! An [`ElementImage`] carries the element's metadata, relation endpoints and
//! every property as a [`DataValue`] produced by the shared `core-types` codec.
//! Conversions never evaluate expressions, infer metadata or substitute
//! defaults: a property that cannot be represented is reported by name.

use std::collections::BTreeMap;

use drasi_core_types::{
    data_value::{DataValue, DataValueError},
    evaluation::variable_value::VariableValue,
    models::{
        ConversionError, Element, ElementMetadata, ElementPropertyMap, ElementReference,
        ElementValue,
    },
};
use serde::{Deserialize, Serialize};

use crate::binary_codec::{
    decode_bounded_messagepack, encode_bounded_messagepack, BinaryEnvelopeCodecError,
};

/// Failure to convert, encode or decode an [`ElementImage`].
#[derive(Debug, thiserror::Error)]
pub enum ElementImageError {
    /// A property of the source element has no data form.
    #[error("element property {property:?} cannot be encoded: {source}")]
    EncodeProperty {
        /// Name of the offending property.
        property: String,
        /// Reason the value cannot be encoded.
        #[source]
        source: DataValueError,
    },
    /// A property of the image is not a valid data value.
    #[error("element image property {property:?} is not a valid data value: {source}")]
    InvalidProperty {
        /// Name of the offending property.
        property: String,
        /// Reason the value is invalid.
        #[source]
        source: DataValueError,
    },
    /// A valid property value has no element property representation.
    #[error("element image property {property:?} cannot be stored as an element property")]
    UnrepresentableProperty {
        /// Name of the offending property.
        property: String,
        #[source]
        source: ConversionError,
    },
    /// The MessagePack frame is malformed, truncated, oversized or has trailing data.
    #[error(transparent)]
    Codec(#[from] BinaryEnvelopeCodecError),
}

/// Lossless data form of a node or relation.
///
/// Property names, labels, identity, effective time and relation direction
/// are kept exactly as stored on the [`Element`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum ElementImage {
    /// A graph node.
    Node {
        /// Identity, labels and effective time.
        metadata: ElementMetadata,
        /// Every property, keyed by name.
        properties: BTreeMap<String, DataValue>,
    },
    /// A directed relation.
    Relation {
        /// Identity, labels and effective time.
        metadata: ElementMetadata,
        /// Start node: `(in_node)-[relation]->(out_node)`.
        in_node: ElementReference,
        /// End node: `(in_node)-[relation]->(out_node)`.
        out_node: ElementReference,
        /// Every property, keyed by name.
        properties: BTreeMap<String, DataValue>,
    },
}

impl ElementImage {
    /// Element identity, labels and effective time.
    pub fn metadata(&self) -> &ElementMetadata {
        match self {
            Self::Node { metadata, .. } | Self::Relation { metadata, .. } => metadata,
        }
    }

    /// Properties keyed by name.
    pub fn properties(&self) -> &BTreeMap<String, DataValue> {
        match self {
            Self::Node { properties, .. } | Self::Relation { properties, .. } => properties,
        }
    }

    /// Check that every property is a valid data value.
    pub fn validate(&self) -> Result<(), ElementImageError> {
        for (property, value) in self.properties() {
            value
                .clone()
                .decode()
                .map_err(|source| ElementImageError::InvalidProperty {
                    property: property.clone(),
                    source,
                })?;
        }
        Ok(())
    }

    /// Encode as one named MessagePack value of at most `limit` bytes.
    ///
    /// The image is validated first, so an invalid value is never written.
    pub fn encode(&self, limit: usize) -> Result<Vec<u8>, ElementImageError> {
        self.validate()?;
        Ok(encode_bounded_messagepack(self, limit)?)
    }

    /// Decode one image of at most `limit` bytes.
    ///
    /// Collection lengths are checked against the remaining input before any
    /// allocation. Truncated input, trailing bytes and invalid property values
    /// are rejected.
    pub fn decode(bytes: &[u8], limit: usize) -> Result<Self, ElementImageError> {
        let image: Self = decode_bounded_messagepack(bytes, limit)?;
        image.validate()?;
        Ok(image)
    }
}

impl TryFrom<&Element> for ElementImage {
    type Error = ElementImageError;

    fn try_from(element: &Element) -> Result<Self, Self::Error> {
        Ok(match element {
            Element::Node {
                metadata,
                properties,
            } => Self::Node {
                metadata: metadata.clone(),
                properties: encode_properties(properties)?,
            },
            Element::Relation {
                metadata,
                in_node,
                out_node,
                properties,
            } => Self::Relation {
                metadata: metadata.clone(),
                in_node: in_node.clone(),
                out_node: out_node.clone(),
                properties: encode_properties(properties)?,
            },
        })
    }
}

impl TryFrom<Element> for ElementImage {
    type Error = ElementImageError;

    fn try_from(element: Element) -> Result<Self, Self::Error> {
        Self::try_from(&element)
    }
}

impl TryFrom<ElementImage> for Element {
    type Error = ElementImageError;

    fn try_from(image: ElementImage) -> Result<Self, Self::Error> {
        Ok(match image {
            ElementImage::Node {
                metadata,
                properties,
            } => Element::Node {
                metadata,
                properties: decode_properties(properties)?,
            },
            ElementImage::Relation {
                metadata,
                in_node,
                out_node,
                properties,
            } => Element::Relation {
                metadata,
                in_node,
                out_node,
                properties: decode_properties(properties)?,
            },
        })
    }
}

impl TryFrom<&ElementImage> for Element {
    type Error = ElementImageError;

    fn try_from(image: &ElementImage) -> Result<Self, Self::Error> {
        Self::try_from(image.clone())
    }
}

fn encode_properties(
    properties: &ElementPropertyMap,
) -> Result<BTreeMap<String, DataValue>, ElementImageError> {
    properties
        .map_iter(|name, value| (name.to_string(), VariableValue::from(value)))
        .map(|(property, value)| match DataValue::encode(&value) {
            Ok(value) => Ok((property, value)),
            Err(source) => Err(ElementImageError::EncodeProperty { property, source }),
        })
        .collect()
}

fn decode_properties(
    properties: BTreeMap<String, DataValue>,
) -> Result<ElementPropertyMap, ElementImageError> {
    let mut decoded = ElementPropertyMap::new();
    for (property, value) in properties {
        let value = match value.decode() {
            Ok(value) => value,
            Err(source) => return Err(ElementImageError::InvalidProperty { property, source }),
        };
        let value: ElementValue = match (&value).try_into() {
            Ok(value) => value,
            Err(source) => {
                return Err(ElementImageError::UnrepresentableProperty { property, source })
            }
        };
        decoded.insert(&property, value);
    }
    Ok(decoded)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
    use drasi_core_types::models::TypedElementValue;
    use ordered_float::OrderedFloat;

    use super::*;

    const LIMIT: usize = 1 << 20;

    fn reference(source: &str, element: &str) -> ElementReference {
        ElementReference::new(source, element)
    }

    fn metadata(element: &str, labels: &[&str], effective_from: u64) -> ElementMetadata {
        ElementMetadata {
            reference: reference("source", element),
            labels: labels.iter().map(|label| Arc::from(*label)).collect(),
            effective_from,
        }
    }

    fn typed(value: DataValue) -> ElementValue {
        ElementValue::Typed(Box::new(TypedElementValue::try_from(value).unwrap()))
    }

    fn date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 2, 28).unwrap()
    }

    fn time() -> NaiveTime {
        NaiveTime::from_hms_nano_opt(23, 59, 58, 123_456_789).unwrap()
    }

    fn zoned() -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339("2026-03-29T01:30:00.5+01:00").unwrap()
    }

    fn rich_properties() -> ElementPropertyMap {
        let mut nested = BTreeMap::new();
        nested.insert("list".to_string(), DataValue::List(vec![DataValue::Null]));
        nested.insert(
            "node".to_string(),
            DataValue::Element(Element::Node {
                metadata: metadata("embedded", &["Inner"], 3),
                properties: {
                    let mut properties = ElementPropertyMap::new();
                    properties.insert("nan", ElementValue::Float(OrderedFloat(f64::NAN)));
                    properties
                },
            }),
        );
        nested.insert(
            "reference".to_string(),
            DataValue::Reference(reference("other", "r")),
        );

        let mut properties = ElementPropertyMap::new();
        properties.insert("", ElementValue::String(Arc::from("empty name")));
        properties.insert("null", ElementValue::Null);
        properties.insert("bool", ElementValue::Bool(true));
        properties.insert("min", ElementValue::Integer(i64::MIN));
        properties.insert("max_signed", ElementValue::Integer(i64::MAX));
        properties.insert("u64_max", typed(DataValue::Unsigned(u64::MAX)));
        properties.insert(
            "nan",
            ElementValue::Float(OrderedFloat(f64::from_bits(0x7ff8_0000_0000_0042))),
        );
        properties.insert("neg_zero", ElementValue::Float(OrderedFloat(-0.0)));
        properties.insert("inf", ElementValue::Float(OrderedFloat(f64::NEG_INFINITY)));
        properties.insert(
            "local",
            ElementValue::LocalDateTime(NaiveDateTime::new(date(), time())),
        );
        properties.insert("offset", ElementValue::ZonedDateTime(zoned()));
        properties.insert(
            "named_zone",
            typed(DataValue::ZonedDateTime(
                zoned(),
                Some("Europe/Paris".to_string()),
            )),
        );
        properties.insert("zoned_time", typed(DataValue::ZonedTime(time(), -19_800)));
        properties.insert("date", typed(DataValue::Date(date())));
        properties.insert(
            "duration",
            typed(DataValue::Duration {
                seconds: -5,
                nanos: -500,
                years: 2,
                months: -3,
            }),
        );
        properties.insert("range", typed(DataValue::Range(None, Some(-1))));
        properties.insert(
            "metadata",
            typed(DataValue::Metadata(metadata("m", &[], 0))),
        );
        properties.insert("object", typed(DataValue::Object(nested)));
        properties.insert(
            "list",
            ElementValue::List(vec![
                ElementValue::Integer(1),
                typed(DataValue::Unsigned(u64::MAX)),
                ElementValue::List(Vec::new()),
            ]),
        );
        properties.insert("awaiting", typed(DataValue::Awaiting));
        properties
    }

    fn node() -> Element {
        Element::Node {
            metadata: metadata("node", &["B", "A", "A"], u64::MAX),
            properties: rich_properties(),
        }
    }

    fn relation() -> Element {
        Element::Relation {
            metadata: metadata("rel", &["KNOWS"], 17),
            in_node: reference("origin-source", "start"),
            out_node: reference("target-source", "end"),
            properties: rich_properties(),
        }
    }

    fn assert_exact_roundtrip(element: Element) {
        let image = ElementImage::try_from(&element).unwrap();
        let bytes = image.encode(LIMIT).unwrap();
        let restored = Element::try_from(&image).unwrap();
        // Private property variants may normalize; every public value bit must survive.
        assert_eq!(
            ElementImage::try_from(&restored)
                .unwrap()
                .encode(LIMIT)
                .unwrap(),
            bytes
        );
        let decoded = ElementImage::decode(&bytes, LIMIT).unwrap();
        assert_eq!(decoded, image);
        let restored = Element::try_from(decoded).unwrap();
        assert_eq!(
            ElementImage::try_from(&restored)
                .unwrap()
                .encode(LIMIT)
                .unwrap(),
            bytes
        );
    }

    #[test]
    fn node_roundtrips_rich_properties() {
        let element = node();
        assert_exact_roundtrip(element.clone());

        let image = ElementImage::try_from(element).unwrap();
        assert!(matches!(image, ElementImage::Node { .. }));
        assert_eq!(image.properties()["u64_max"], DataValue::Unsigned(u64::MAX));
        assert_eq!(
            image.properties()["nan"],
            DataValue::Float(0x7ff8_0000_0000_0042)
        );
        assert_eq!(
            image.properties()["neg_zero"],
            DataValue::Float((-0.0_f64).to_bits())
        );
        assert_eq!(
            image.properties()["named_zone"],
            DataValue::ZonedDateTime(zoned(), Some("Europe/Paris".to_string()))
        );
        assert_eq!(
            image.properties()["zoned_time"],
            DataValue::ZonedTime(time(), -19_800)
        );
        assert_eq!(
            image.properties().len(),
            rich_properties().map_iter(|_, _| ()).count()
        );
    }

    #[test]
    fn relation_preserves_endpoints_and_metadata() {
        assert_exact_roundtrip(relation());

        let image = ElementImage::try_from(relation()).unwrap();
        let ElementImage::Relation {
            metadata,
            in_node,
            out_node,
            ..
        } = &image
        else {
            panic!("expected a relation image");
        };
        assert_eq!(in_node, &reference("origin-source", "start"));
        assert_eq!(out_node, &reference("target-source", "end"));
        assert_eq!(metadata.reference, reference("source", "rel"));
        assert_eq!(metadata.effective_from, 17);
        assert_eq!(metadata.labels.len(), 1);

        let swapped = match image.clone() {
            ElementImage::Relation {
                metadata,
                in_node,
                out_node,
                properties,
            } => ElementImage::Relation {
                metadata,
                in_node: out_node,
                out_node: in_node,
                properties,
            },
            ElementImage::Node { .. } => unreachable!(),
        };
        assert_ne!(swapped, image);
    }

    #[test]
    fn labels_keep_order_and_duplicates() {
        let image = ElementImage::try_from(node()).unwrap();
        let labels: Vec<&str> = image.metadata().labels.iter().map(|l| &**l).collect();
        assert_eq!(labels, ["B", "A", "A"]);
        assert_eq!(image.metadata().effective_from, u64::MAX);
    }

    #[test]
    fn empty_property_maps_roundtrip() {
        for element in [
            Element::Node {
                metadata: metadata("n", &[], 0),
                properties: ElementPropertyMap::new(),
            },
            Element::Relation {
                metadata: metadata("r", &[], 0),
                in_node: reference("s", "a"),
                out_node: reference("s", "b"),
                properties: ElementPropertyMap::new(),
            },
        ] {
            assert!(ElementImage::try_from(&element)
                .unwrap()
                .properties()
                .is_empty());
            assert_exact_roundtrip(element);
        }
    }

    fn invalid_image() -> ElementImage {
        let mut properties = BTreeMap::new();
        properties.insert("fine".to_string(), DataValue::Bool(true));
        properties.insert(
            "bad_duration".to_string(),
            DataValue::List(vec![DataValue::Duration {
                seconds: 0,
                nanos: 1_000_000_000,
                years: 0,
                months: 0,
            }]),
        );
        ElementImage::Node {
            metadata: metadata("bad", &[], 1),
            properties,
        }
    }

    #[test]
    fn invalid_values_name_the_property() {
        let image = invalid_image();
        assert!(matches!(
            image.validate(),
            Err(ElementImageError::InvalidProperty { ref property, .. }) if property == "bad_duration"
        ));
        assert!(matches!(
            Element::try_from(&image),
            Err(ElementImageError::InvalidProperty { ref property, source })
                if property == "bad_duration" && source == DataValueError::InvalidDurationRemainder
        ));
        assert!(matches!(
            image.encode(LIMIT),
            Err(ElementImageError::InvalidProperty { .. })
        ));
    }

    #[test]
    fn decode_rejects_semantically_invalid_values() {
        let bytes = encode_bounded_messagepack(&invalid_image(), LIMIT).unwrap();
        assert!(matches!(
            ElementImage::decode(&bytes, LIMIT),
            Err(ElementImageError::InvalidProperty { property, .. }) if property == "bad_duration"
        ));

        let mut properties = BTreeMap::new();
        properties.insert("offset".to_string(), DataValue::ZonedTime(time(), 200_000));
        let image = ElementImage::Node {
            metadata: metadata("bad", &[], 1),
            properties,
        };
        let bytes = encode_bounded_messagepack(&image, LIMIT).unwrap();
        assert!(matches!(
            ElementImage::decode(&bytes, LIMIT),
            Err(ElementImageError::InvalidProperty { .. })
        ));
    }

    #[test]
    fn decode_rejects_truncated_and_trailing_bytes() {
        let bytes = ElementImage::try_from(relation())
            .unwrap()
            .encode(LIMIT)
            .unwrap();
        for end in [0, 1, bytes.len() / 2, bytes.len() - 1] {
            assert!(
                ElementImage::decode(&bytes[..end], LIMIT).is_err(),
                "accepted {end} of {} bytes",
                bytes.len()
            );
        }

        let mut trailing = bytes.clone();
        trailing.push(0xc0);
        assert!(matches!(
            ElementImage::decode(&trailing, LIMIT),
            Err(ElementImageError::Codec(
                BinaryEnvelopeCodecError::InvalidFrame("trailing bytes")
            ))
        ));
        let mut doubled = bytes.clone();
        doubled.extend_from_slice(&bytes);
        assert!(ElementImage::decode(&doubled, LIMIT).is_err());
    }

    #[test]
    fn decode_rejects_hostile_lengths_and_garbage() {
        // Declares four billion array entries with no input to back them.
        for bytes in [
            &[0xdd, 0xff, 0xff, 0xff, 0xff][..],
            &[0xdf, 0xff, 0xff, 0xff, 0xff][..],
            &[0xc1][..],
            &[0xc0][..],
            &[0x81, 0xa4, b'N', b'o', b'n', b'e', 0xc0][..],
            &[][..],
        ] {
            assert!(ElementImage::decode(bytes, LIMIT).is_err());
        }
    }

    #[test]
    fn encode_and_decode_enforce_the_size_limit() {
        let image = ElementImage::try_from(node()).unwrap();
        let bytes = image.encode(LIMIT).unwrap();
        assert!(matches!(
            image.encode(bytes.len() - 1),
            Err(ElementImageError::Codec(
                BinaryEnvelopeCodecError::SizeLimit { .. }
            ))
        ));
        assert!(matches!(
            ElementImage::decode(&bytes, bytes.len() - 1),
            Err(ElementImageError::Codec(
                BinaryEnvelopeCodecError::SizeLimit { .. }
            ))
        ));
        assert!(ElementImage::decode(&bytes, bytes.len()).is_ok());
    }

    #[test]
    fn encoding_is_deterministic() {
        let first = ElementImage::try_from(node())
            .unwrap()
            .encode(LIMIT)
            .unwrap();
        let second = ElementImage::try_from(node())
            .unwrap()
            .encode(LIMIT)
            .unwrap();
        assert_eq!(first, second);
    }
}
