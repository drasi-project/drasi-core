// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Reading and validating planar geometries from element properties.

use anyhow::{anyhow, bail, ensure, Context, Result};
use drasi_core_types::models::{ElementPropertyMap, ElementValue};
use geo::{
    BoundingRect, Coord, Geometry, LineString, MultiLineString, MultiPoint, MultiPolygon, Point,
    Polygon, Rect, Validation,
};
use serde_json::Value;

use crate::config::{GeometrySource, RingClosure};

/// The validation rules shared by every set.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Rules {
    pub epsilon: f64,
    pub ring_closure: RingClosure,
    pub max_vertices: usize,
}

/// A validated geometry and its bounding rectangle.
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub(crate) struct Shape {
    pub geometry: Geometry<f64>,
    pub bounds: Rect<f64>,
}

/// Reads one member geometry. Every error names the offending property.
pub(crate) fn extract(
    source: &GeometrySource,
    properties: &ElementPropertyMap,
    rules: &Rules,
) -> Result<Shape> {
    let mut reader = Reader { rules, vertices: 0 };
    let geometry = match source {
        GeometrySource::Point {
            property: Some(property),
            ..
        } => Geometry::Point(Point(reader.coord(&json(properties, property)?, false)?)),
        GeometrySource::Point {
            x: Some(x),
            y: Some(y),
            ..
        } => Geometry::Point(Point(Coord {
            x: finite(&scalar(properties, x)?, x)?,
            y: finite(&scalar(properties, y)?, y)?,
        })),
        GeometrySource::Point { .. } => bail!("point geometry is not configured"),
        GeometrySource::LineString { property } => Geometry::LineString(
            reader
                .line_string(&json(properties, property)?, false)
                .with_context(|| format!("property '{property}'"))?,
        ),
        GeometrySource::Polygon { property } => {
            let value = json(properties, property)?;
            let rings = match value.as_array().and_then(|items| items.first()) {
                Some(Value::Array(first))
                    if first
                        .first()
                        .is_some_and(|item| item.is_array() || item.is_object()) =>
                {
                    value
                }
                _ => Value::Array(vec![value]),
            };
            Geometry::Polygon(
                reader
                    .polygon(&rings, false)
                    .with_context(|| format!("property '{property}'"))?,
            )
        }
        GeometrySource::GeoJson { property } => reader
            .geo_json(&json(properties, property)?)
            .with_context(|| format!("GeoJSON property '{property}'"))?,
    };
    geometry
        .check_validation()
        .map_err(|error| anyhow!("invalid geometry: {error}"))?;
    let bounds = geometry.bounding_rect().context("geometry has no extent")?;
    Ok(Shape { geometry, bounds })
}

/// A finite numeric property, used for coordinates and thresholds.
pub(crate) fn number(properties: &ElementPropertyMap, name: &str) -> Result<f64> {
    finite(&scalar(properties, name)?, name)
}

fn scalar(properties: &ElementPropertyMap, name: &str) -> Result<Value> {
    match properties.get(name) {
        None | Some(ElementValue::Null) => bail!("missing property '{name}'"),
        Some(value) => Ok(Value::from(value)),
    }
}

fn finite(value: &Value, name: &str) -> Result<f64> {
    let number = value
        .as_f64()
        .with_context(|| format!("property '{name}' is not a number"))?;
    ensure!(number.is_finite(), "property '{name}' is not finite");
    Ok(number)
}

/// The property as JSON; text is parsed, so JSON-encoded values are accepted.
fn json(properties: &ElementPropertyMap, name: &str) -> Result<Value> {
    match scalar(properties, name)? {
        Value::String(text) => {
            serde_json::from_str(&text).with_context(|| format!("property '{name}' is not JSON"))
        }
        value => Ok(value),
    }
}

struct Reader<'a> {
    rules: &'a Rules,
    vertices: usize,
}

impl Reader<'_> {
    /// `[x, y]` or `{x, y}`. GeoJSON positions may carry ignored extra ordinates.
    fn coord(&mut self, value: &Value, geo_json: bool) -> Result<Coord<f64>> {
        self.vertices += 1;
        ensure!(
            self.vertices <= self.rules.max_vertices,
            "geometry has more than limits.maxVertices ({}) vertices",
            self.rules.max_vertices
        );
        let (x, y) = match value {
            Value::Array(items) if items.len() == 2 || (geo_json && items.len() > 2) => {
                (&items[0], &items[1])
            }
            Value::Object(fields) if !geo_json && fields.len() == 2 => (
                fields.get("x").context("coordinate object needs x")?,
                fields.get("y").context("coordinate object needs y")?,
            ),
            _ => bail!("a coordinate must be [x, y] or {{\"x\": .., \"y\": ..}}"),
        };
        let x = x.as_f64().context("coordinate x is not a number")?;
        let y = y.as_f64().context("coordinate y is not a number")?;
        ensure!(x.is_finite() && y.is_finite(), "coordinates must be finite");
        Ok(Coord { x, y })
    }

    fn coords(&mut self, value: &Value, geo_json: bool) -> Result<Vec<Coord<f64>>> {
        value
            .as_array()
            .context("expected an array of coordinates")?
            .iter()
            .map(|item| self.coord(item, geo_json))
            .collect()
    }

    fn line_string(&mut self, value: &Value, geo_json: bool) -> Result<LineString<f64>> {
        let coords = self.coords(value, geo_json)?;
        ensure!(coords.len() >= 2, "a line string needs at least 2 points");
        self.distinct_edges(&coords, false)?;
        Ok(LineString::new(coords))
    }

    fn polygon(&mut self, value: &Value, geo_json: bool) -> Result<Polygon<f64>> {
        let mut rings = value
            .as_array()
            .context("expected an array of rings")?
            .iter()
            .map(|ring| self.ring(ring, geo_json))
            .collect::<Result<Vec<_>>>()?
            .into_iter();
        let exterior = rings.next().context("a polygon needs an exterior ring")?;
        Ok(Polygon::new(exterior, rings.collect()))
    }

    fn ring(&mut self, value: &Value, geo_json: bool) -> Result<LineString<f64>> {
        let mut coords = self.coords(value, geo_json)?;
        let closed = coords.len() > 1 && self.same(coords[0], coords[coords.len() - 1]);
        ensure!(
            closed || self.rules.ring_closure == RingClosure::Auto,
            "polygon rings must repeat their first coordinate"
        );
        if closed {
            coords.pop();
        }
        ensure!(
            coords.len() >= 3,
            "a polygon ring needs at least 3 distinct vertices"
        );
        self.distinct_edges(&coords, true)?;
        // Twice the area over the perimeter: the mean width of the ring.
        let (mut area, mut perimeter) = (0.0, 0.0);
        for (index, a) in coords.iter().enumerate() {
            let b = coords[(index + 1) % coords.len()];
            area += a.x * b.y - b.x * a.y;
            perimeter += (b.x - a.x).hypot(b.y - a.y);
        }
        let width = area.abs() / perimeter;
        ensure!(
            width > 0.0 && width > self.rules.epsilon,
            "a polygon ring must enclose an area (its vertices are collinear within epsilon {})",
            self.rules.epsilon
        );
        Ok(LineString::new(coords))
    }

    fn geo_json(&mut self, value: &Value) -> Result<Geometry<f64>> {
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .context("a GeoJSON object needs a string type")?;
        if kind == "Feature" {
            let geometry = value
                .get("geometry")
                .context("a Feature needs a geometry")?;
            return self.geo_json(geometry);
        }
        let coordinates = value
            .get("coordinates")
            .context("a GeoJSON geometry needs coordinates")?;
        let array = || {
            coordinates
                .as_array()
                .context("coordinates must be an array")
        };
        Ok(match kind {
            "Point" => Geometry::Point(Point(self.coord(coordinates, true)?)),
            "MultiPoint" => Geometry::MultiPoint(MultiPoint::new(
                array()?
                    .iter()
                    .map(|item| self.coord(item, true).map(Point))
                    .collect::<Result<_>>()?,
            )),
            "LineString" => Geometry::LineString(self.line_string(coordinates, true)?),
            "MultiLineString" => Geometry::MultiLineString(MultiLineString::new(
                array()?
                    .iter()
                    .map(|item| self.line_string(item, true))
                    .collect::<Result<_>>()?,
            )),
            "Polygon" => Geometry::Polygon(self.polygon(coordinates, true)?),
            "MultiPolygon" => Geometry::MultiPolygon(MultiPolygon::new(
                array()?
                    .iter()
                    .map(|item| self.polygon(item, true))
                    .collect::<Result<_>>()?,
            )),
            other => bail!("unsupported GeoJSON type '{other}'"),
        })
    }

    fn same(&self, a: Coord<f64>, b: Coord<f64>) -> bool {
        (a.x - b.x).hypot(a.y - b.y) <= self.rules.epsilon
    }

    fn distinct_edges(&self, coords: &[Coord<f64>], ring: bool) -> Result<()> {
        let count = coords.len();
        let edges = if ring { count } else { count - 1 };
        for index in 0..edges {
            let next = (index + 1) % count;
            ensure!(
                !self.same(coords[index], coords[next]),
                "zero-length edge between vertices {index} and {next} (epsilon {})",
                self.rules.epsilon
            );
        }
        Ok(())
    }
}
