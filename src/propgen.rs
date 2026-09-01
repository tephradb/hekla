//! Type-directed generators for the conversion-table properties.
//!
//! Every conversion in hekla is keyed on a declaration, so a generator that produced a
//! value and a type independently would spend most of its budget on pairs no program
//! could declare. These generate a [`Type`] and derive everything else from it: the
//! [`FieldKind`] through [`FieldKind::of`], which is many-to-one, and the value through
//! [`value_for`], which is total for the types it admits.
//!
//! **Uniform strategies are the wrong default here.** `any::<i64>()` reaches `i64::MIN`
//! with probability 2^-64, and every interesting bug in a conversion table lives at a
//! boundary: the widest integer, the empty string, the exact `@max`, a scale of zero.
//! So each type mixes a hand-written edge list with a uniform tail, weighted heavily
//! toward the edges.

use std::sync::LazyLock;

use heklang::Value;
use heklang::ir::{EnumDef, RecordDef, RecordField, Type};
use heklang::value::Defs;
use proptest::prelude::*;

use crate::schema::FieldKind;

/// The scale ceiling these generators observe.
///
/// Not a property of the type: `Money(n)` parses for any `n` up to 255, and
/// `scaled::text` then evaluates `10u64.pow(n)`, which overflows above 19. Bounded here
/// so the round trips test the range a program can actually use, with the boundary
/// itself pinned as a named case rather than found repeatedly by the generator.
pub const MAX_SCALE: u8 = 18;

/// The declarations a generated value resolves against: one enum and one record, the
/// same shapes heklang's own conversion-table test uses.
static ENUMS: LazyLock<Vec<EnumDef>> = LazyLock::new(|| {
    vec![EnumDef {
        name: "Tier".to_owned(),
        variants: vec!["Free".to_owned(), "Paid".to_owned()],
        default: Some(0),
    }]
});

static RECORDS: LazyLock<Vec<RecordDef>> = LazyLock::new(|| {
    vec![RecordDef {
        name: "Line".to_owned(),
        module: None,
        fields: vec![
            RecordField {
                name: "sku".to_owned(),
                ty: Type::String,
                max_len: Some(20),
            },
            RecordField {
                name: "qty".to_owned(),
                ty: Type::Int,
                max_len: None,
            },
            RecordField {
                name: "price".to_owned(),
                ty: Type::Money(3),
                max_len: None,
            },
            RecordField {
                name: "note".to_owned(),
                ty: Type::opt(Type::String),
                max_len: None,
            },
        ],
    }]
});

pub fn defs() -> Defs<'static> {
    Defs {
        local: &[],
        enums: &ENUMS,
        records: &RECORDS,
    }
}

/// Epoch micros `column_form` can render as RFC 3339, which is `time`'s own range.
/// Outside it the conversion falls through unchanged and stops being a round trip; that
/// is a real behaviour with a test of its own rather than something to generate into.
pub const MIN_MICROS: i64 = -62_167_219_200_000_000;
pub const MAX_MICROS: i64 = 253_402_300_799_999_999;

/// Every type a declared field can carry, and that both directions of a conversion have
/// an arm for.
///
/// `Rounding`, `Response` and `Outcome` are absent because `Value::from_json` has no arm
/// for them: the checker keeps them off a field long before one could hold one, and a
/// property asserting they fail belongs beside them rather than in the round trip.
pub fn field_type() -> impl Strategy<Value = Type> {
    let leaf = prop_oneof![
        Just(Type::Bool),
        Just(Type::Int),
        Just(Type::String),
        Just(Type::Uuid),
        Just(Type::Timestamp),
        Just(Type::Json),
        Just(Type::Enum("Tier".to_owned())),
        Just(Type::Record("Line".to_owned())),
        (0u8..=MAX_SCALE).prop_map(Type::Money),
        (0u8..=MAX_SCALE).prop_map(Type::Decimal),
    ];
    leaf.prop_recursive(2, 8, 2, |inner| {
        prop_oneof![
            // An `Opt` never wraps an `Opt`: `String?` is `Opt(String)` and there is no
            // `String??` to declare. It matters beyond tidiness, because `FieldKind::base`
            // peels one level, so a doubly-optional kind would fall through every table
            // keyed on it and the properties below would fail on a shape no program has.
            2 => inner.clone().prop_map(|ty| match ty {
                Type::Opt(_) => ty,
                other => Type::opt(other),
            }),
            1 => inner.clone().prop_map(Type::list),
            1 => inner.prop_map(|value| Type::map(Type::String, value)),
        ]
    })
}

/// The kind hekla derives from a declared type. Derived rather than generated, because
/// [`FieldKind::of`] is many-to-one and an independently drawn pair would be a shape no
/// declaration can produce.
pub fn kind_of(ty: &Type) -> FieldKind {
    FieldKind::of(ty, defs())
}

/// A value of the given type, biased toward the boundaries.
pub fn value_for(ty: &Type) -> BoxedStrategy<Value> {
    match ty {
        Type::Bool => any::<bool>().prop_map(Value::Bool).boxed(),
        Type::Int => int().prop_map(Value::Int).boxed(),
        Type::String => text().prop_map(Value::str).boxed(),
        Type::Uuid => uuid().prop_map(Value::uuid).boxed(),
        Type::Timestamp => micros().prop_map(Value::Timestamp).boxed(),
        Type::Money(scale) => {
            let scale = *scale;
            units(scale)
                .prop_map(move |units| Value::money(units, scale))
                .boxed()
        }
        Type::Decimal(scale) => {
            let scale = *scale;
            units(scale)
                .prop_map(move |units| Value::decimal(units, scale))
                .boxed()
        }
        Type::Enum(name) => {
            let name = name.clone();
            prop_oneof![Just("Free".to_owned()), Just("Paid".to_owned())]
                .prop_map(move |variant| Value::Enum {
                    ty: name.clone(),
                    variant,
                })
                .boxed()
        }
        Type::Record(name) => record(name.clone()).boxed(),
        Type::Json => json().prop_map(Value::Json).boxed(),
        Type::Opt(inner) => {
            let inner = (**inner).clone();
            let empty = inner.clone();
            prop_oneof![
                1 => Just(Value::none(empty)),
                // A JSON null inside a `Json?` is the one value an optional cannot tell
                // from absence: both are `null` on the wire, and rule 8 reads that back
                // as `none`. Left out here and pinned by name in `heklang_host`, because
                // it is an answer to a real ambiguity rather than something a round trip
                // could repair.
                3 => value_for(&inner)
                    .prop_filter("a json null cannot be told from none", |value| {
                        !matches!(value, Value::Json(heklang::Json::Null))
                    })
                    .prop_map(Value::some),
            ]
            .boxed()
        }
        Type::List(inner) => {
            let inner = (**inner).clone();
            let declared = inner.clone();
            prop::collection::vec(value_for(&inner), 0..3)
                .prop_map(move |items| Value::list(declared.clone(), items))
                .boxed()
        }
        Type::Map(key, value) => {
            // Keys are drawn from a small pool on purpose: a map's JSON form is keyed by
            // rendered text, so two keys that render alike would lose an entry and the
            // round trip would fail for a reason that is not a bug.
            let value_ty = (**value).clone();
            let key_ty = (**key).clone();
            prop::collection::btree_map(
                prop_oneof![
                    Just("a".to_owned()),
                    Just("b".to_owned()),
                    Just("c".to_owned())
                ],
                value_for(&value_ty),
                0..3,
            )
            .prop_map(move |entries| {
                Value::map(
                    key_ty.clone(),
                    value_ty.clone(),
                    entries
                        .into_iter()
                        .map(|(key, value)| (heklang::Key::Str(key.into()), value)),
                )
            })
            .boxed()
        }
        // Not generated: see `field_type`.
        other => panic!("no generator for {other}"),
    }
}

/// A type paired with a value of it, which is what every round-trip property takes.
pub fn typed_value() -> impl Strategy<Value = (Type, Value)> {
    field_type().prop_flat_map(|ty| {
        let declared = ty.clone();
        value_for(&ty).prop_map(move |value| (declared.clone(), value))
    })
}

fn int() -> impl Strategy<Value = i64> {
    prop_oneof![
        4 => prop_oneof![
            Just(i64::MIN),
            Just(i64::MAX),
            Just(-1),
            Just(0),
            Just(1),
            Just(i64::from(i32::MAX) + 1),
        ],
        1 => any::<i64>(),
    ]
}

/// Units that survive their own scale. `scaled::text` renders `units` at `scale`, and
/// re-reading it multiplies back up, so a value near `i64::MAX` at a wide scale is an
/// overflow rather than a round trip. Bounding the units by the scale keeps the property
/// about the conversion instead of about arithmetic that is already checked.
fn units(scale: u8) -> impl Strategy<Value = i64> {
    let bound = 10i64
        .checked_pow(u32::from(18 - scale.min(18)))
        .unwrap_or(1);
    prop_oneof![
        4 => prop_oneof![Just(0i64), Just(1), Just(-1), Just(bound - 1), Just(1 - bound)],
        1 => -bound..bound,
    ]
}

fn micros() -> impl Strategy<Value = i64> {
    prop_oneof![
        4 => prop_oneof![
            Just(0i64),
            Just(1),
            Just(-1),
            Just(MIN_MICROS),
            Just(MAX_MICROS),
            Just(1_700_000_000_000_000),
        ],
        1 => MIN_MICROS..=MAX_MICROS,
    ]
}

fn text() -> impl Strategy<Value = String> {
    prop_oneof![
        4 => prop_oneof![
            Just(String::new()),
            Just(" ".to_owned()),
            Just("\"".to_owned()),
            Just("\\".to_owned()),
            Just("\n\r\t".to_owned()),
            Just("\u{0}".to_owned()),
            Just("日本語".to_owned()),
            // One grapheme, several code points, which is where a naive length check
            // and a byte count part company.
            Just("\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}".to_owned()),
            Just("' OR 1=1--".to_owned()),
            // Text that looks like another JSON type. Harmless for a `String` field and
            // the whole question for a `Json` one, where flattening a seal drops the
            // quotes that said which it was.
            Just("42".to_owned()),
            Just("true".to_owned()),
            Just("null".to_owned()),
            Just("[1]".to_owned()),
        ],
        1 => ".{0,32}",
    ]
}

fn uuid() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("00000000-0000-0000-0000-000000000000".to_owned()),
        Just("ffffffff-ffff-ffff-ffff-ffffffffffff".to_owned()),
        Just("FFFFFFFF-FFFF-FFFF-FFFF-FFFFFFFFFFFF".to_owned()),
        Just("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".to_owned()),
        Just("0190d1a1-0000-7000-8000-000000000001".to_owned()),
    ]
}

fn record(name: String) -> impl Strategy<Value = Value> {
    (text(), int(), units(3), prop::option::of(text())).prop_map(move |(sku, qty, price, note)| {
        Value::record(
            name.clone(),
            [
                ("sku", Value::str(sku)),
                ("qty", Value::Int(qty)),
                ("price", Value::money(price, 3)),
                (
                    "note",
                    match note {
                        Some(text) => Value::some(Value::str(text)),
                        None => Value::none(Type::String),
                    },
                ),
            ],
        )
    })
}

/// Arbitrary JSON, including the number texts that normalise on a round trip through
/// serde. Those are the point: `Json` is the one field type that carries a caller's
/// bytes rather than a declared shape.
pub fn json() -> impl Strategy<Value = heklang::Json> {
    let leaf = prop_oneof![
        Just(heklang::Json::Null),
        any::<bool>().prop_map(heklang::Json::Bool),
        number().prop_map(heklang::Json::Num),
        text().prop_map(heklang::Json::Str),
    ];
    leaf.prop_recursive(3, 12, 3, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..3).prop_map(heklang::Json::Arr),
            prop::collection::btree_map("[a-z]{1,3}", inner, 0..3).prop_map(heklang::Json::Obj),
        ]
    })
}

/// JSON number text, in serde's own normal form. The forms that do *not* survive a
/// round trip (`1e2`, `1E5`, `-0`) are pinned as named cases instead, because a
/// generator that emitted them would fail a property that is telling the truth.
fn number() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("0".to_owned()),
        Just("-1".to_owned()),
        Just("10.50".to_owned()),
        Just("1.0".to_owned()),
        Just("1e+2".to_owned()),
        Just("1e-2".to_owned()),
        Just("-0.0".to_owned()),
        // Wider than an f64 holds, which is what `arbitrary_precision` is carried for.
        Just("123456789012345678901234567890".to_owned()),
        Just("0.123456789012345678901234567890".to_owned()),
        int().prop_map(|value| value.to_string()),
    ]
}
