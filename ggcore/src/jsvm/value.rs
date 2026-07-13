//! NaN-boxed value representation: every JS value is 8 bytes.
//!
//! Real doubles are stored verbatim; arithmetic NaNs are canonicalized to
//! one quiet-NaN bit pattern (0x7FF8_0000_0000_0000). Everything else
//! lives in the NaN space *above* any bit pattern a hardware double can
//! produce: the top 16 bits select a tag (>= 0xFFF9), the low 48 bits
//! carry the payload — an i32, a special constant, or a 32-bit index
//! into a heap arena / the DOM arena.
//!
//! Layout of the tag space:
//!   <= 0xFFF8  double (canonical NaN, infinities, all finite values)
//!      0xFFF9  int32
//!      0xFFFA  special: undefined / null / false / true
//!      0xFFFB  string      (heap index)
//!      0xFFFC  object      (heap index)
//!      0xFFFD  function    (closure index)
//!      0xFFFE  DOM node    (arena index — DOM calls skip any binding layer)
//!      0xFFFF  cell        (boxed captured variable; VM-internal)

use std::fmt;

/// Derived equality is *bit identity* (useful for tests and interning),
/// not JS `===`: -0.0 != 0.0 here, and NaN == NaN (both canonical).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Value(u64);

const CANON_NAN: u64 = 0x7FF8_0000_0000_0000;
const TAG_SHIFT: u32 = 48;
const PAYLOAD: u64 = 0x0000_FFFF_FFFF_FFFF;

const TAG_INT: u64 = 0xFFF9;
const TAG_SPECIAL: u64 = 0xFFFA;
const TAG_STR: u64 = 0xFFFB;
const TAG_OBJ: u64 = 0xFFFC;
const TAG_FUNC: u64 = 0xFFFD;
const TAG_DOM: u64 = 0xFFFE;
const TAG_CELL: u64 = 0xFFFF;

const SP_UNDEFINED: u64 = 0;
const SP_NULL: u64 = 1;
const SP_FALSE: u64 = 2;
const SP_TRUE: u64 = 3;
/// TDZ marker: the value of a `let`/`const` binding before its
/// declaration runs. Never observable from JS — reads that could see
/// it are guarded by a TdzCheck that throws instead.
const SP_TDZ: u64 = 4;

impl Value {
    pub const UNDEFINED: Value =
        Value((TAG_SPECIAL << TAG_SHIFT) | SP_UNDEFINED);
    pub const NULL: Value = Value((TAG_SPECIAL << TAG_SHIFT) | SP_NULL);
    pub const FALSE: Value = Value((TAG_SPECIAL << TAG_SHIFT) | SP_FALSE);
    pub const TRUE: Value = Value((TAG_SPECIAL << TAG_SHIFT) | SP_TRUE);
    pub const TDZ: Value = Value((TAG_SPECIAL << TAG_SHIFT) | SP_TDZ);

    #[inline]
    fn tagged(tag: u64, payload: u64) -> Value {
        Value((tag << TAG_SHIFT) | (payload & PAYLOAD))
    }

    #[inline]
    fn tag(self) -> u64 {
        self.0 >> TAG_SHIFT
    }

    #[inline]
    pub fn number(n: f64) -> Value {
        if n.is_nan() {
            Value(CANON_NAN)
        } else {
            Value(n.to_bits())
        }
    }

    #[inline]
    pub fn int(i: i32) -> Value {
        Value::tagged(TAG_INT, i as u32 as u64)
    }

    #[inline]
    pub fn boolean(b: bool) -> Value {
        if b { Value::TRUE } else { Value::FALSE }
    }

    #[inline]
    pub fn string(idx: u32) -> Value {
        Value::tagged(TAG_STR, idx as u64)
    }

    #[inline]
    pub fn object(idx: u32) -> Value {
        Value::tagged(TAG_OBJ, idx as u64)
    }

    #[inline]
    pub fn function(idx: u32) -> Value {
        Value::tagged(TAG_FUNC, idx as u64)
    }

    #[inline]
    pub fn dom_node(idx: u32) -> Value {
        Value::tagged(TAG_DOM, idx as u64)
    }

    #[inline]
    pub fn cell(idx: u32) -> Value {
        Value::tagged(TAG_CELL, idx as u64)
    }

    #[inline]
    pub fn is_double(self) -> bool {
        self.tag() < TAG_INT
    }

    #[inline]
    pub fn is_int(self) -> bool {
        self.tag() == TAG_INT
    }

    #[inline]
    pub fn is_number(self) -> bool {
        self.tag() <= TAG_INT
    }

    #[inline]
    pub fn is_undefined(self) -> bool {
        self == Value::UNDEFINED
    }

    #[inline]
    pub fn is_null(self) -> bool {
        self == Value::NULL
    }

    #[inline]
    pub fn is_nullish(self) -> bool {
        self.tag() == TAG_SPECIAL && (self.0 & PAYLOAD) <= SP_NULL
    }

    #[inline]
    pub fn is_boolean(self) -> bool {
        self == Value::FALSE || self == Value::TRUE
    }

    #[inline]
    pub fn is_tdz(self) -> bool {
        self == Value::TDZ
    }

    #[inline]
    pub fn is_string(self) -> bool {
        self.tag() == TAG_STR
    }

    #[inline]
    pub fn is_object(self) -> bool {
        self.tag() == TAG_OBJ
    }

    #[inline]
    pub fn is_function(self) -> bool {
        self.tag() == TAG_FUNC
    }

    #[inline]
    pub fn is_dom_node(self) -> bool {
        self.tag() == TAG_DOM
    }

    #[inline]
    pub fn is_cell(self) -> bool {
        self.tag() == TAG_CELL
    }

    /// Only meaningful when `is_double()`.
    #[inline]
    pub fn as_f64(self) -> f64 {
        f64::from_bits(self.0)
    }

    /// Only meaningful when `is_int()`.
    #[inline]
    pub fn as_i32(self) -> i32 {
        self.0 as u32 as i32
    }

    /// Only meaningful when `is_boolean()`.
    #[inline]
    pub fn as_bool(self) -> bool {
        self == Value::TRUE
    }

    /// Heap/arena index for string / object / function / DOM tags.
    #[inline]
    pub fn index(self) -> u32 {
        self.0 as u32
    }

    /// Numeric value of an int or double (the hot-path unification).
    #[inline]
    pub fn to_number_raw(self) -> f64 {
        if self.is_int() {
            self.as_i32() as f64
        } else {
            self.as_f64()
        }
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.tag() {
            TAG_INT => write!(f, "Int({})", self.as_i32()),
            TAG_SPECIAL => match *self {
                Value::UNDEFINED => write!(f, "undefined"),
                Value::NULL => write!(f, "null"),
                Value::FALSE => write!(f, "false"),
                Value::TRUE => write!(f, "true"),
                Value::TDZ => write!(f, "<uninitialized>"),
                _ => write!(f, "Special(?{})", self.0 & PAYLOAD),
            },
            TAG_STR => write!(f, "Str(#{})", self.index()),
            TAG_OBJ => write!(f, "Obj(#{})", self.index()),
            TAG_FUNC => write!(f, "Func(#{})", self.index()),
            TAG_DOM => write!(f, "Dom(#{})", self.index()),
            TAG_CELL => write!(f, "Cell(#{})", self.index()),
            _ => write!(f, "Num({})", self.as_f64()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_is_8_bytes() {
        assert_eq!(std::mem::size_of::<Value>(), 8);
    }

    #[test]
    fn double_roundtrip() {
        for n in [0.0, -0.0, 1.5, -273.15, f64::MAX, f64::MIN_POSITIVE,
                  f64::INFINITY, f64::NEG_INFINITY] {
            let v = Value::number(n);
            assert!(v.is_double() && v.is_number(), "{n}");
            assert_eq!(v.as_f64().to_bits(), n.to_bits(), "{n}");
        }
    }

    #[test]
    fn nan_is_canonicalized() {
        let crafted = f64::from_bits(0xFFF8_DEAD_BEEF_0001);
        assert!(crafted.is_nan());
        for n in [f64::NAN, -f64::NAN, 0.0 / 0.0, crafted] {
            let v = Value::number(n);
            assert!(v.is_double());
            assert!(v.as_f64().is_nan());
            assert_eq!(v.0, 0x7FF8_0000_0000_0000);
        }
    }

    #[test]
    fn int_roundtrip() {
        for i in [0, 1, -1, 42, i32::MAX, i32::MIN] {
            let v = Value::int(i);
            assert!(v.is_int() && v.is_number() && !v.is_double(), "{i}");
            assert_eq!(v.as_i32(), i);
            assert_eq!(v.to_number_raw(), i as f64);
        }
    }

    #[test]
    fn specials_are_distinct_and_not_numbers() {
        let all = [Value::UNDEFINED, Value::NULL, Value::FALSE, Value::TRUE];
        for (i, a) in all.iter().enumerate() {
            assert!(!a.is_number() && !a.is_double() && !a.is_int());
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
        assert!(Value::UNDEFINED.is_nullish() && Value::NULL.is_nullish());
        assert!(!Value::FALSE.is_nullish() && !Value::TRUE.is_nullish());
        assert!(Value::TRUE.as_bool() && !Value::FALSE.as_bool());
        assert!(Value::TDZ.is_tdz());
        assert!(!Value::TDZ.is_boolean() && !Value::TDZ.is_nullish());
        for v in all {
            assert!(!v.is_tdz());
        }
    }

    #[test]
    fn index_tags_are_disjoint() {
        let vals = [Value::string(7), Value::object(7),
                    Value::function(7), Value::dom_node(7), Value::cell(7)];
        let preds: [fn(Value) -> bool; 5] = [
            Value::is_string, Value::is_object,
            Value::is_function, Value::is_dom_node, Value::is_cell,
        ];
        for (i, v) in vals.iter().enumerate() {
            assert_eq!(v.index(), 7);
            assert!(!v.is_number());
            for (j, p) in preds.iter().enumerate() {
                assert_eq!(p(*v), i == j, "tag {i} vs pred {j}");
            }
        }
        assert_eq!(Value::dom_node(u32::MAX).index(), u32::MAX);
    }
}
