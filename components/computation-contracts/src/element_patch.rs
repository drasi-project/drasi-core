// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Property-level update of a graph [`Element`].
//!
//! An [`ElementPatch`] carries the metadata and relation endpoints of the new
//! element version together with the properties to set and the names of the
//! properties to remove. Properties that are named in neither place stay
//! unchanged. The helper performs no I/O and makes no completeness or snapshot
//! claim. Change ordering, timestamp admission, transactions, publication and
//! source or query runtime policy belong to the caller.

use std::collections::BTreeSet;

use drasi_core_types::models::{Element, ElementPropertyMap, ElementReference};
use serde::{Deserialize, Serialize};

use crate::binary_codec::{
    decode_bounded_messagepack, encode_bounded_messagepack, BinaryEnvelopeCodecError,
};
use crate::element_image::{ElementImage, ElementImageError};

/// Whether an element is a node or a relation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElementPatchKind {
    /// A graph node.
    Node,
    /// A directed relation.
    Relation,
}

impl ElementPatchKind {
    fn of_image(image: &ElementImage) -> Self {
        match image {
            ElementImage::Node { .. } => Self::Node,
            ElementImage::Relation { .. } => Self::Relation,
        }
    }

    fn of_element(element: &Element) -> Self {
        match element {
            Element::Node { .. } => Self::Node,
            Element::Relation { .. } => Self::Relation,
        }
    }
}

impl std::fmt::Display for ElementPatchKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Node => "node",
            Self::Relation => "relation",
        })
    }
}

/// Failure to build, encode, decode or apply an [`ElementPatch`].
#[derive(Debug, thiserror::Error)]
pub enum ElementPatchError {
    /// The patch image holds an invalid or unrepresentable property.
    #[error("element patch image is invalid: {0}")]
    Image(#[from] ElementImageError),
    /// A property is both set and removed.
    #[error("element patch for {element} both sets and removes property {property:?}")]
    ConflictingProperty {
        /// Element the patch targets.
        element: ElementReference,
        /// Name of the property named in both places.
        property: String,
    },
    /// The patch targets a different element than the base.
    #[error("element patch for {patch} cannot be applied to base element {base}")]
    ReferenceMismatch {
        /// Reference of the supplied base element.
        base: ElementReference,
        /// Reference the patch targets.
        patch: ElementReference,
    },
    /// The patch and the base are not the same kind of element.
    #[error("element patch for {element} is a {patch} but the base element is a {base}")]
    KindMismatch {
        /// Element the patch targets.
        element: ElementReference,
        /// Kind of the supplied base element.
        base: ElementPatchKind,
        /// Kind of the patch.
        patch: ElementPatchKind,
    },
    /// The MessagePack frame is malformed, truncated, oversized or has trailing data.
    #[error(transparent)]
    Codec(#[from] BinaryEnvelopeCodecError),
}

/// Property-level update of one node or relation.
///
/// The image carries the new metadata and relation endpoints exactly as a
/// `SourceChange::Update`-style element would. Its property map is a **patch**,
/// not the full record: every entry is a property to set, and absence means
/// "unchanged". Setting a property to `Null` stores `Null`; it does not remove
/// the property. Removal is expressed only by the separate removal set.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ElementPatch {
    image: ElementImage,
    remove: BTreeSet<String>,
}

impl ElementPatch {
    /// Build a validated patch from the image of properties to set and the
    /// names of properties to remove.
    pub fn try_new(
        image: ElementImage,
        remove: BTreeSet<String>,
    ) -> Result<Self, ElementPatchError> {
        let patch = Self { image, remove };
        patch.validate()?;
        Ok(patch)
    }

    /// Build a patch from a normal update element: every property of the
    /// element is set and nothing is removed.
    pub fn from_update_element(element: &Element) -> Result<Self, ElementPatchError> {
        Self::try_new(ElementImage::try_from(element)?, BTreeSet::new())
    }

    /// New metadata and relation endpoints, with the properties to set.
    ///
    /// The property map of this image is a patch, not the full record.
    pub fn image(&self) -> &ElementImage {
        &self.image
    }

    /// Names of the properties to remove.
    pub fn removed_properties(&self) -> &BTreeSet<String> {
        &self.remove
    }

    /// Reference of the targeted element.
    pub fn reference(&self) -> &ElementReference {
        &self.image.metadata().reference
    }

    /// Check that every set value is valid and no property is both set and removed.
    pub fn validate(&self) -> Result<(), ElementPatchError> {
        self.image.validate()?;
        let set = self.image.properties();
        if let Some(property) = self.remove.iter().find(|name| set.contains_key(*name)) {
            return Err(ElementPatchError::ConflictingProperty {
                element: self.reference().clone(),
                property: property.clone(),
            });
        }
        Ok(())
    }

    /// Encode as one named MessagePack value of at most `limit` bytes.
    ///
    /// The patch is validated first, so an invalid patch is never written.
    pub fn encode(&self, limit: usize) -> Result<Vec<u8>, ElementPatchError> {
        self.validate()?;
        Ok(encode_bounded_messagepack(self, limit)?)
    }

    /// Decode one patch of at most `limit` bytes.
    ///
    /// Truncated input, trailing bytes, invalid property values and a property
    /// that is both set and removed are rejected.
    pub fn decode(bytes: &[u8], limit: usize) -> Result<Self, ElementPatchError> {
        let patch: Self = decode_bounded_messagepack(bytes, limit)?;
        patch.validate()?;
        Ok(patch)
    }

    /// Produce the element after the patch.
    ///
    /// With a base, the base must be the same element reference and kind. The
    /// result keeps the base properties except those removed, then sets the
    /// patch properties; metadata and relation endpoints come from the patch.
    ///
    /// Without a base, the result is the element built from the properties to
    /// set; removing a property that is absent is a no-op. Neither case claims
    /// that the result is a complete record.
    pub fn apply_to(&self, base: Option<&Element>) -> Result<Element, ElementPatchError> {
        self.validate()?;
        let patch = Element::try_from(&self.image)?;
        let Some(base) = base else {
            return Ok(patch);
        };

        if base.get_reference() != self.reference() {
            return Err(ElementPatchError::ReferenceMismatch {
                base: base.get_reference().clone(),
                patch: self.reference().clone(),
            });
        }
        let base_kind = ElementPatchKind::of_element(base);
        let patch_kind = ElementPatchKind::of_image(&self.image);
        if base_kind != patch_kind {
            return Err(ElementPatchError::KindMismatch {
                element: self.reference().clone(),
                base: base_kind,
                patch: patch_kind,
            });
        }

        let mut properties = ElementPropertyMap::new();
        for (name, value) in base
            .get_properties()
            .map_iter(|name, value| (name.clone(), value.clone()))
        {
            if !self.remove.contains(&*name) {
                properties.insert(&name, value);
            }
        }
        for (name, value) in patch
            .get_properties()
            .map_iter(|name, value| (name.clone(), value.clone()))
        {
            properties.insert(&name, value);
        }

        Ok(match patch {
            Element::Node { metadata, .. } => Element::Node {
                metadata,
                properties,
            },
            Element::Relation {
                metadata,
                in_node,
                out_node,
                ..
            } => Element::Relation {
                metadata,
                in_node,
                out_node,
                properties,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use chrono::{DateTime, FixedOffset, NaiveDate, NaiveTime};
    use drasi_core_types::{
        data_value::DataValue,
        models::{ElementMetadata, ElementValue, TypedElementValue},
    };

    use super::*;

    const LIMIT: usize = 1 << 20;

    fn reference(element: &str) -> ElementReference {
        ElementReference::new("source", element)
    }

    fn metadata(element: &str, labels: &[&str], effective_from: u64) -> ElementMetadata {
        ElementMetadata {
            reference: reference(element),
            labels: labels.iter().map(|label| Arc::from(*label)).collect(),
            effective_from,
        }
    }

    fn typed(value: DataValue) -> ElementValue {
        ElementValue::Typed(Box::new(TypedElementValue::try_from(value).unwrap()))
    }

    fn string(value: &str) -> ElementValue {
        ElementValue::String(Arc::from(value))
    }

    fn properties(entries: Vec<(&str, ElementValue)>) -> ElementPropertyMap {
        let mut map = ElementPropertyMap::new();
        for (name, value) in entries {
            map.insert(name, value);
        }
        map
    }

    fn node(element: &str, entries: Vec<(&str, ElementValue)>) -> Element {
        Element::Node {
            metadata: metadata(element, &["Old"], 10),
            properties: properties(entries),
        }
    }

    fn relation(element: &str, entries: Vec<(&str, ElementValue)>) -> Element {
        Element::Relation {
            metadata: metadata(element, &["KNOWS"], 10),
            in_node: reference("start"),
            out_node: reference("end"),
            properties: properties(entries),
        }
    }

    fn node_image(element: &str, labels: &[&str], set: Vec<(&str, DataValue)>) -> ElementImage {
        ElementImage::Node {
            metadata: metadata(element, labels, 20),
            properties: set
                .into_iter()
                .map(|(name, value)| (name.to_string(), value))
                .collect(),
        }
    }

    fn names(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    fn patch(image: ElementImage, remove: &[&str]) -> ElementPatch {
        ElementPatch::try_new(image, names(remove)).unwrap()
    }

    fn property<'a>(element: &'a Element, name: &str) -> Option<&'a ElementValue> {
        element.get_properties().get(name)
    }

    fn property_names(element: &Element) -> Vec<String> {
        let mut names: Vec<String> = element
            .get_properties()
            .map_iter(|name, _| name.to_string())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn set_preserves_omitted_properties() {
        let base = node(
            "n",
            vec![
                ("keep", ElementValue::Integer(1)),
                ("change", string("before")),
            ],
        );
        let image = node_image(
            "n",
            &["New"],
            vec![
                ("change", DataValue::String("after".to_string())),
                ("added", DataValue::Bool(true)),
            ],
        );

        let result = patch(image, &[]).apply_to(Some(&base)).unwrap();

        assert_eq!(property_names(&result), ["added", "change", "keep"]);
        assert_eq!(property(&result, "keep"), Some(&ElementValue::Integer(1)));
        assert_eq!(property(&result, "change"), Some(&string("after")));
        assert_eq!(property(&result, "added"), Some(&ElementValue::Bool(true)));
        let metadata = result.get_metadata();
        assert_eq!(
            metadata.labels.iter().map(|l| &**l).collect::<Vec<_>>(),
            ["New"]
        );
        assert_eq!(metadata.effective_from, 20);
    }

    #[test]
    fn explicit_remove_deletes_and_absent_remove_is_a_noop() {
        let base = node(
            "n",
            vec![
                ("gone", ElementValue::Integer(1)),
                ("keep", ElementValue::Integer(2)),
            ],
        );
        let image = node_image("n", &[], Vec::new());

        let result = patch(image, &["gone", "never_existed"])
            .apply_to(Some(&base))
            .unwrap();

        assert_eq!(property_names(&result), ["keep"]);
        assert_eq!(property(&result, "gone"), None);
    }

    #[test]
    fn null_is_a_property_not_a_removal() {
        let base = node("n", vec![("a", ElementValue::Integer(1))]);
        let image = node_image("n", &[], vec![("a", DataValue::Null)]);

        let result = patch(image, &[]).apply_to(Some(&base)).unwrap();

        assert_eq!(property(&result, "a"), Some(&ElementValue::Null));
        assert_eq!(property_names(&result), ["a"]);
    }

    #[test]
    fn same_key_set_and_remove_fails() {
        let image = node_image("n", &[], vec![("a", DataValue::Null)]);
        let error = ElementPatch::try_new(image, names(&["a"])).unwrap_err();
        assert!(matches!(
            error,
            ElementPatchError::ConflictingProperty { ref property, ref element }
                if property == "a" && *element == reference("n")
        ));
        assert!(error.to_string().contains("\"a\""));

        let conflicting = ElementPatch {
            image: node_image("n", &[], vec![("a", DataValue::Bool(false))]),
            remove: names(&["a"]),
        };
        assert!(matches!(
            conflicting.validate(),
            Err(ElementPatchError::ConflictingProperty { .. })
        ));
        assert!(matches!(
            conflicting.apply_to(None),
            Err(ElementPatchError::ConflictingProperty { .. })
        ));
        assert!(matches!(
            conflicting.encode(LIMIT),
            Err(ElementPatchError::ConflictingProperty { .. })
        ));
        let bytes = encode_bounded_messagepack(&conflicting, LIMIT).unwrap();
        assert!(matches!(
            ElementPatch::decode(&bytes, LIMIT),
            Err(ElementPatchError::ConflictingProperty { .. })
        ));
    }

    #[test]
    fn reference_mismatch_fails() {
        let base = node("other", vec![("a", ElementValue::Integer(1))]);
        let image = node_image("n", &[], Vec::new());
        assert!(matches!(
            patch(image.clone(), &[]).apply_to(Some(&base)),
            Err(ElementPatchError::ReferenceMismatch { base, patch })
                if base == reference("other") && patch == reference("n")
        ));

        let other_source = Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("elsewhere", "n"),
                labels: Arc::from(Vec::<Arc<str>>::new()),
                effective_from: 1,
            },
            properties: ElementPropertyMap::new(),
        };
        assert!(matches!(
            patch(image, &[]).apply_to(Some(&other_source)),
            Err(ElementPatchError::ReferenceMismatch { .. })
        ));
    }

    #[test]
    fn kind_mismatch_fails_in_both_directions() {
        let node_patch = patch(node_image("x", &[], Vec::new()), &[]);
        let relation_base = relation("x", Vec::new());
        assert!(matches!(
            node_patch.apply_to(Some(&relation_base)),
            Err(ElementPatchError::KindMismatch {
                base: ElementPatchKind::Relation,
                patch: ElementPatchKind::Node,
                ..
            })
        ));

        let relation_patch = ElementPatch::from_update_element(&relation("x", Vec::new())).unwrap();
        assert!(matches!(
            relation_patch.apply_to(Some(&node("x", Vec::new()))),
            Err(ElementPatchError::KindMismatch {
                base: ElementPatchKind::Node,
                patch: ElementPatchKind::Relation,
                ..
            })
        ));
    }

    #[test]
    fn relation_endpoints_and_metadata_come_from_the_patch() {
        let base = relation(
            "r",
            vec![
                ("since", ElementValue::Integer(2000)),
                ("w", ElementValue::Integer(1)),
            ],
        );
        let image = ElementImage::Relation {
            metadata: metadata("r", &["FOLLOWS", "KNOWS"], 99),
            in_node: ElementReference::new("other", "b"),
            out_node: ElementReference::new("other", "a"),
            properties: BTreeMap::from([("w".to_string(), DataValue::Unsigned(2))]),
        };

        let result = patch(image, &["since"]).apply_to(Some(&base)).unwrap();

        let Element::Relation {
            metadata,
            in_node,
            out_node,
            properties,
        } = result
        else {
            panic!("expected a relation");
        };
        assert_eq!(in_node, ElementReference::new("other", "b"));
        assert_eq!(out_node, ElementReference::new("other", "a"));
        assert_eq!(metadata.reference, reference("r"));
        assert_eq!(metadata.effective_from, 99);
        assert_eq!(
            metadata.labels.iter().map(|l| &**l).collect::<Vec<_>>(),
            ["FOLLOWS", "KNOWS"]
        );
        assert_eq!(properties.get("w"), Some(&ElementValue::Integer(2)));
        assert_eq!(properties.get("since"), None);
    }

    #[test]
    fn missing_base_upserts_the_supplied_element() {
        let update = relation(
            "r",
            vec![("a", ElementValue::Integer(1)), ("b", string("x"))],
        );
        let patch = ElementPatch::from_update_element(&update).unwrap();
        assert!(patch.removed_properties().is_empty());

        let result = patch.apply_to(None).unwrap();
        assert_eq!(
            ElementImage::try_from(&result).unwrap(),
            ElementImage::try_from(&update).unwrap()
        );

        let with_removal = ElementPatch::try_new(
            node_image("n", &["L"], vec![("a", DataValue::Unsigned(1))]),
            names(&["absent"]),
        )
        .unwrap();
        let result = with_removal.apply_to(None).unwrap();
        assert_eq!(property_names(&result), ["a"]);
    }

    #[test]
    fn from_update_element_replaces_every_supplied_property() {
        let base = node(
            "n",
            vec![
                ("a", ElementValue::Integer(1)),
                ("b", ElementValue::Integer(2)),
            ],
        );
        let update = node("n", vec![("a", ElementValue::Integer(9))]);

        let result = ElementPatch::from_update_element(&update)
            .unwrap()
            .apply_to(Some(&base))
            .unwrap();

        assert_eq!(property(&result, "a"), Some(&ElementValue::Integer(9)));
        assert_eq!(property(&result, "b"), Some(&ElementValue::Integer(2)));
    }

    #[test]
    fn rich_values_survive_apply_and_roundtrip() {
        let zoned =
            DateTime::<FixedOffset>::parse_from_rfc3339("2026-03-29T01:30:00.5+01:00").unwrap();
        let time = NaiveTime::from_hms_nano_opt(23, 59, 58, 123_456_789).unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 2, 28).unwrap();
        let nested = DataValue::Object(BTreeMap::from([
            (
                "list".to_string(),
                DataValue::List(vec![DataValue::Unsigned(u64::MAX), DataValue::Null]),
            ),
            (
                "ref".to_string(),
                DataValue::Reference(reference("elsewhere")),
            ),
        ]));
        let set = vec![
            ("u64", DataValue::Unsigned(u64::MAX)),
            (
                "named_zone",
                DataValue::ZonedDateTime(zoned, Some("Europe/Paris".to_string())),
            ),
            ("zoned_time", DataValue::ZonedTime(time, -19_800)),
            (
                "duration",
                DataValue::Duration {
                    seconds: -5,
                    nanos: -500,
                    years: 2,
                    months: -3,
                },
            ),
            ("nested", nested.clone()),
        ];
        let base_date = typed(DataValue::Date(date));
        let base = node(
            "n",
            vec![
                ("kept_date", base_date.clone()),
                ("kept_u64", typed(DataValue::Unsigned(7))),
            ],
        );

        let built = patch(node_image("n", &[], set.clone()), &["missing"]);
        let bytes = built.encode(LIMIT).unwrap();
        let decoded = ElementPatch::decode(&bytes, LIMIT).unwrap();
        assert_eq!(decoded, built);

        let result = decoded.apply_to(Some(&base)).unwrap();
        assert_eq!(property(&result, "kept_date"), Some(&base_date));
        assert_eq!(
            property(&result, "kept_u64"),
            Some(&typed(DataValue::Unsigned(7)))
        );
        let image = ElementImage::try_from(&result).unwrap();
        for (name, value) in set {
            assert_eq!(image.properties()[name], value, "property {name}");
        }
        assert_eq!(image.properties()["kept_date"], DataValue::Date(date));
    }

    #[test]
    fn encode_is_deterministic_and_enforces_the_limit() {
        let built = patch(
            node_image("n", &[], vec![("a", DataValue::Unsigned(1))]),
            &["b", "c"],
        );
        let bytes = built.encode(LIMIT).unwrap();
        assert_eq!(bytes, built.encode(LIMIT).unwrap());
        assert!(matches!(
            built.encode(bytes.len() - 1),
            Err(ElementPatchError::Codec(
                BinaryEnvelopeCodecError::SizeLimit { .. }
            ))
        ));
        assert!(matches!(
            ElementPatch::decode(&bytes, bytes.len() - 1),
            Err(ElementPatchError::Codec(
                BinaryEnvelopeCodecError::SizeLimit { .. }
            ))
        ));
        assert_eq!(ElementPatch::decode(&bytes, bytes.len()).unwrap(), built);
    }

    #[test]
    fn decode_rejects_truncated_trailing_and_garbage_payloads() {
        let bytes = ElementPatch::from_update_element(&relation(
            "r",
            vec![("a", ElementValue::Integer(1))],
        ))
        .unwrap()
        .encode(LIMIT)
        .unwrap();
        for end in [0, 1, bytes.len() / 2, bytes.len() - 1] {
            assert!(
                ElementPatch::decode(&bytes[..end], LIMIT).is_err(),
                "accepted {end} of {} bytes",
                bytes.len()
            );
        }

        let mut trailing = bytes.clone();
        trailing.push(0xc0);
        assert!(matches!(
            ElementPatch::decode(&trailing, LIMIT),
            Err(ElementPatchError::Codec(
                BinaryEnvelopeCodecError::InvalidFrame("trailing bytes")
            ))
        ));
        let mut doubled = bytes.clone();
        doubled.extend_from_slice(&bytes);
        assert!(ElementPatch::decode(&doubled, LIMIT).is_err());

        for garbage in [
            &[0xdd, 0xff, 0xff, 0xff, 0xff][..],
            &[0xc1][..],
            &[0xc0][..],
            &[][..],
        ] {
            assert!(ElementPatch::decode(garbage, LIMIT).is_err());
        }
    }

    fn invalid_image() -> ElementImage {
        node_image(
            "n",
            &[],
            vec![
                ("fine", DataValue::Bool(true)),
                (
                    "bad_duration",
                    DataValue::List(vec![DataValue::Duration {
                        seconds: 0,
                        nanos: 1_000_000_000,
                        years: 0,
                        months: 0,
                    }]),
                ),
            ],
        )
    }

    #[test]
    fn invalid_data_values_fail_everywhere_with_the_property_name() {
        fn assert_invalid(result: Result<impl std::fmt::Debug, ElementPatchError>) {
            assert!(
                matches!(
                    result,
                    Err(ElementPatchError::Image(ElementImageError::InvalidProperty { ref property, .. }))
                        if property == "bad_duration"
                ),
                "{result:?}"
            );
        }

        assert_invalid(ElementPatch::try_new(invalid_image(), BTreeSet::new()));

        let invalid = ElementPatch {
            image: invalid_image(),
            remove: BTreeSet::new(),
        };
        assert_invalid(invalid.validate());
        assert_invalid(invalid.encode(LIMIT));
        assert_invalid(invalid.apply_to(None));
        assert_invalid(invalid.apply_to(Some(&node("n", Vec::new()))));

        let bytes = encode_bounded_messagepack(&invalid, LIMIT).unwrap();
        assert_invalid(ElementPatch::decode(&bytes, LIMIT));
    }

    #[test]
    fn apply_does_not_modify_the_base() {
        let base = node("n", vec![("a", ElementValue::Integer(1))]);
        let before = base.clone();
        patch(
            node_image("n", &[], vec![("a", DataValue::Unsigned(2))]),
            &[],
        )
        .apply_to(Some(&base))
        .unwrap();
        assert_eq!(base, before);
    }
}
