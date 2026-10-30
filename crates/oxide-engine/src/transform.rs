//! Production's partial writes (superset only, see docs/superset-backlog.md): a mutation's
//! property mask and its property transforms, applied when the mutation is committed.
//!
//! The mask merges the written entity into the stored one: masked properties come from the
//! written entity (or are removed when it lacks them), the others are kept. Transforms then apply
//! in order, each to the result of the previous one, and report the value they produced (null
//! for the array transforms).

use oxide_model::{Entity, Value, ValueKind};

/// A property path: names of nested entity properties, outermost first.
pub type Path = Vec<String>;

#[derive(Clone, Debug, PartialEq)]
pub struct Transform {
    pub property: Path,
    pub op: TransformOp,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TransformOp {
    /// `set_to_server_value: REQUEST_TIME`.
    RequestTime,
    Increment(Number),
    Maximum(Number),
    Minimum(Number),
    AppendMissing(Vec<Value>),
    RemoveAll(Vec<Value>),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Number {
    Integer(i64),
    Double(f64),
}

impl Number {
    fn of(v: &ValueKind) -> Option<Number> {
        match v {
            ValueKind::Integer(i) => Some(Number::Integer(*i)),
            ValueKind::Double(d) => Some(Number::Double(*d)),
            _ => None,
        }
    }

    fn as_f64(self) -> f64 {
        match self {
            Number::Integer(i) => i as f64,
            Number::Double(d) => d,
        }
    }

    fn kind(self) -> ValueKind {
        match self {
            Number::Integer(i) => ValueKind::Integer(i),
            Number::Double(d) => ValueKind::Double(d),
        }
    }

    /// Numeric comparison across integers and doubles (NaN aside).
    fn cmp(self, other: Number) -> std::cmp::Ordering {
        match (self, other) {
            (Number::Integer(a), Number::Integer(b)) => a.cmp(&b),
            _ => self
                .as_f64()
                .partial_cmp(&other.as_f64())
                .unwrap_or(std::cmp::Ordering::Equal),
        }
    }

    fn is_nan(self) -> bool {
        matches!(self, Number::Double(d) if d.is_nan())
    }
}

/// The partial-write instructions of one mutation.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PartialWrite {
    pub mask: Option<Vec<Path>>,
    pub transforms: Vec<Transform>,
}

impl PartialWrite {
    pub fn is_empty(&self) -> bool {
        self.mask.is_none() && self.transforms.is_empty()
    }

    /// The entity to store, given the one already stored, and the transform results.
    /// `request_time` is in microseconds.
    pub fn apply(
        &self,
        existing: Option<&Entity>,
        written: Entity,
        request_time: i64,
    ) -> (Entity, Vec<Value>) {
        let mut entity = match (&self.mask, existing) {
            (Some(mask), Some(stored)) => {
                let mut merged = stored.clone();
                merged.key = written.key.clone();
                merged.v3 = None;
                for path in mask {
                    match get(&written, path) {
                        Some(v) => set(&mut merged, path, v.clone()),
                        None => remove(&mut merged, path),
                    }
                }
                merged
            }
            (Some(mask), None) => {
                let mut fresh = Entity {
                    key: written.key.clone(),
                    ..Default::default()
                };
                for path in mask {
                    if let Some(v) = get(&written, path) {
                        set(&mut fresh, path, v.clone());
                    }
                }
                fresh
            }
            (None, _) => written,
        };
        let mut results = Vec::with_capacity(self.transforms.len());
        for t in &self.transforms {
            let current = get(&entity, &t.property);
            let keep_flags = |kind: ValueKind| Value {
                kind,
                meaning: 0,
                exclude_from_indexes: current.is_some_and(|c| c.exclude_from_indexes),
            };
            let current_number = current.and_then(|c| Number::of(&c.kind));
            let current_array = || match current.map(|c| &c.kind) {
                Some(ValueKind::Array(a)) => a.clone(),
                _ => Vec::new(),
            };
            let (new, result) = match &t.op {
                TransformOp::RequestTime => {
                    let v = keep_flags(ValueKind::Timestamp(request_time / 1000 * 1000));
                    (v.clone(), v)
                }
                TransformOp::Increment(n) => {
                    let v = keep_flags(match current_number {
                        Some(c) => add(c, *n).kind(),
                        None => n.kind(),
                    });
                    (v.clone(), v)
                }
                TransformOp::Maximum(n) | TransformOp::Minimum(n) => {
                    let max = matches!(t.op, TransformOp::Maximum(_));
                    let kind = match current_number {
                        None => n.kind(),
                        Some(c) if c.is_nan() => c.kind(),
                        Some(_) if n.is_nan() => n.kind(),
                        Some(c) => {
                            let ord = n.cmp(c);
                            let replace = if max { ord.is_gt() } else { ord.is_lt() };
                            if replace { n.kind() } else { c.kind() }
                        }
                    };
                    let v = keep_flags(kind);
                    (v.clone(), v)
                }
                TransformOp::AppendMissing(values) => {
                    let mut array = current_array();
                    for v in values {
                        if !array.iter().any(|e| equivalent(&e.kind, &v.kind)) {
                            array.push(v.clone());
                        }
                    }
                    (
                        keep_flags(ValueKind::Array(array)),
                        Value::new(ValueKind::Null),
                    )
                }
                TransformOp::RemoveAll(values) => {
                    let mut array = current_array();
                    array.retain(|e| !values.iter().any(|v| equivalent(&e.kind, &v.kind)));
                    (
                        keep_flags(ValueKind::Array(array)),
                        Value::new(ValueKind::Null),
                    )
                }
            };
            set(&mut entity, &t.property, new);
            results.push(result);
        }
        (entity, results)
    }
}

/// Integer addition saturates; anything involving a double is a double.
fn add(a: Number, b: Number) -> Number {
    match (a, b) {
        (Number::Integer(x), Number::Integer(y)) => Number::Integer(x.saturating_add(y)),
        _ => Number::Double(a.as_f64() + b.as_f64()),
    }
}

/// Array-transform equality: numbers compare across types, NaN equals NaN.
fn equivalent(a: &ValueKind, b: &ValueKind) -> bool {
    match (Number::of(a), Number::of(b)) {
        (Some(x), Some(y)) => {
            (x.is_nan() && y.is_nan()) || (!x.is_nan() && !y.is_nan() && x.cmp(y).is_eq())
        }
        (None, None) => a == b,
        _ => false,
    }
}

fn get<'e>(entity: &'e Entity, path: &[String]) -> Option<&'e Value> {
    let (first, rest) = path.split_first()?;
    let value = entity.properties.get(first)?;
    if rest.is_empty() {
        return Some(value);
    }
    match &value.kind {
        ValueKind::Entity(e) => get(e, rest),
        _ => None,
    }
}

/// Sets the value at `path`, replacing anything on the way that is not an entity.
fn set(entity: &mut Entity, path: &[String], value: Value) {
    let Some((first, rest)) = path.split_first() else {
        return;
    };
    if rest.is_empty() {
        entity.properties.insert(first.clone(), value);
        return;
    }
    let slot = entity.properties.get_or_insert_with(first.clone(), || {
        Value::new(ValueKind::Entity(Box::default()))
    });
    if !matches!(slot.kind, ValueKind::Entity(_)) {
        *slot = Value::new(ValueKind::Entity(Box::default()));
    }
    if let ValueKind::Entity(e) = &mut slot.kind {
        set(e, rest, value);
    }
}

fn remove(entity: &mut Entity, path: &[String]) {
    let Some((first, rest)) = path.split_first() else {
        return;
    };
    if rest.is_empty() {
        entity.properties.remove(first);
    } else if let Some(Value {
        kind: ValueKind::Entity(e),
        ..
    }) = entity.properties.get_mut(first)
    {
        remove(e, rest);
    }
}

/// Splits a property path at unescaped dots; a backslash escapes the next character.
pub fn parse_path(path: &str) -> Option<Path> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut chars = path.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => current.push(chars.next()?),
            '.' => parts.push(std::mem::take(&mut current)),
            c => current.push(c),
        }
    }
    parts.push(current);
    parts.iter().all(|p| !p.is_empty()).then_some(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity(props: &[(&str, ValueKind)]) -> Entity {
        let mut e = Entity::default();
        for (n, v) in props {
            e.properties.insert(n.to_string(), Value::new(v.clone()));
        }
        e
    }

    fn path(p: &str) -> Path {
        parse_path(p).unwrap()
    }

    fn run(existing: Option<&Entity>, written: Entity, w: PartialWrite) -> (Entity, Vec<Value>) {
        w.apply(existing, written, 1_234_567)
    }

    #[test]
    fn paths() {
        assert_eq!(parse_path("a.b").unwrap(), ["a", "b"]);
        assert_eq!(parse_path(r"a\.b").unwrap(), ["a.b"]);
        assert_eq!(parse_path(r"a\\.b").unwrap(), [r"a\", "b"]);
        assert!(parse_path("a..b").is_none());
        assert!(parse_path("").is_none());
    }

    #[test]
    fn mask_merges_into_the_stored_entity() {
        use ValueKind::*;
        let stored = entity(&[("a", Integer(1)), ("b", Integer(2)), ("c", Integer(3))]);
        let written = entity(&[("a", Integer(9)), ("z", Integer(0))]);
        let (e, _) = run(
            Some(&stored),
            written.clone(),
            PartialWrite {
                mask: Some(vec![path("a"), path("b")]),
                transforms: vec![],
            },
        );
        assert_eq!(e, entity(&[("a", Integer(9)), ("c", Integer(3))]));
        let (fresh, _) = run(
            None,
            written,
            PartialWrite {
                mask: Some(vec![path("a"), path("b")]),
                transforms: vec![],
            },
        );
        assert_eq!(fresh, entity(&[("a", Integer(9))]));
    }

    #[test]
    fn numeric_transforms() {
        use ValueKind::*;
        let t = |p: &str, op| Transform {
            property: path(p),
            op,
        };
        let (e, results) = run(
            None,
            entity(&[
                ("i", Integer(i64::MAX - 1)),
                ("d", Double(1.5)),
                ("s", String("x".into())),
                ("m", Integer(3)),
            ]),
            PartialWrite {
                mask: None,
                transforms: vec![
                    t("i", TransformOp::Increment(Number::Integer(5))),
                    t("d", TransformOp::Increment(Number::Integer(1))),
                    t("s", TransformOp::Increment(Number::Integer(2))),
                    t("new.n", TransformOp::Increment(Number::Double(0.5))),
                    t("m", TransformOp::Maximum(Number::Double(3.0))),
                    t("m", TransformOp::Minimum(Number::Double(2.5))),
                    t("ts", TransformOp::RequestTime),
                ],
            },
        );
        let kinds: Vec<ValueKind> = results.into_iter().map(|v| v.kind).collect();
        assert_eq!(
            kinds,
            [
                Integer(i64::MAX),
                Double(2.5),
                Integer(2),
                Double(0.5),
                Integer(3),
                Double(2.5),
                Timestamp(1_234_000),
            ]
        );
        assert!(matches!(
            &e.properties["new"].kind,
            Entity(inner) if inner.properties["n"].kind == Double(0.5)
        ));
    }

    #[test]
    fn array_transforms() {
        use ValueKind::*;
        let arr = |v: Vec<ValueKind>| Array(v.into_iter().map(Value::new).collect());
        let t = |op| Transform {
            property: path("a"),
            op,
        };
        let (e, results) = run(
            None,
            entity(&[("a", arr(vec![Integer(1), Double(2.0), Null]))]),
            PartialWrite {
                mask: None,
                transforms: vec![
                    t(TransformOp::AppendMissing(
                        [Double(1.0), Integer(3), Integer(3), Null]
                            .into_iter()
                            .map(Value::new)
                            .collect(),
                    )),
                    t(TransformOp::RemoveAll(vec![Value::new(Integer(2))])),
                ],
            },
        );
        assert_eq!(
            e.properties["a"].kind,
            arr(vec![Integer(1), Null, Integer(3)])
        );
        assert!(results.iter().all(|r| r.kind == Null));
    }
}
