//! A `serde::Serializer` that builds a `serde_json::Value` but refuses non-finite floats and
//! non-string map keys, so `Hash::of_canonical_json` can never silently hash a `null` where a
//! `NaN` was (`event-schema.md` §3.8).

use serde::ser::{self, Serialize};
use serde_json::{Map, Number, Value};

use super::HashError;

/// Serialize `v` into a `Value`, rejecting non-finite floats and non-string map keys.
pub fn to_strict_value<T: Serialize + ?Sized>(v: &T) -> Result<Value, HashError> {
    v.serialize(Strict)
}

impl ser::Error for HashError {
    fn custom<T: std::fmt::Display>(msg: T) -> Self {
        HashError::NotCanonicalizable(msg.to_string())
    }
}

struct Strict;

fn float(f: f64) -> Result<Value, HashError> {
    if !f.is_finite() {
        return Err(HashError::NotCanonicalizable(format!(
            "non-finite float {f}"
        )));
    }
    Number::from_f64(f)
        .map(Value::Number)
        .ok_or_else(|| HashError::NotCanonicalizable(format!("float {f}")))
}

impl ser::Serializer for Strict {
    type Ok = Value;
    type Error = HashError;
    type SerializeSeq = SeqS;
    type SerializeTuple = SeqS;
    type SerializeTupleStruct = SeqS;
    type SerializeTupleVariant = VariantSeqS;
    type SerializeMap = MapS;
    type SerializeStruct = MapS;
    type SerializeStructVariant = VariantMapS;

    fn serialize_bool(self, v: bool) -> Result<Value, HashError> {
        Ok(Value::Bool(v))
    }
    fn serialize_i8(self, v: i8) -> Result<Value, HashError> {
        Ok(Value::from(v))
    }
    fn serialize_i16(self, v: i16) -> Result<Value, HashError> {
        Ok(Value::from(v))
    }
    fn serialize_i32(self, v: i32) -> Result<Value, HashError> {
        Ok(Value::from(v))
    }
    fn serialize_i64(self, v: i64) -> Result<Value, HashError> {
        Ok(Value::from(v))
    }
    fn serialize_i128(self, v: i128) -> Result<Value, HashError> {
        i64::try_from(v)
            .map(Value::from)
            .map_err(|_| HashError::NotCanonicalizable(format!("integer {v} out of range")))
    }
    fn serialize_u8(self, v: u8) -> Result<Value, HashError> {
        Ok(Value::from(v))
    }
    fn serialize_u16(self, v: u16) -> Result<Value, HashError> {
        Ok(Value::from(v))
    }
    fn serialize_u32(self, v: u32) -> Result<Value, HashError> {
        Ok(Value::from(v))
    }
    fn serialize_u64(self, v: u64) -> Result<Value, HashError> {
        Ok(Value::from(v))
    }
    fn serialize_u128(self, v: u128) -> Result<Value, HashError> {
        u64::try_from(v)
            .map(Value::from)
            .map_err(|_| HashError::NotCanonicalizable(format!("integer {v} out of range")))
    }
    fn serialize_f32(self, v: f32) -> Result<Value, HashError> {
        float(f64::from(v))
    }
    fn serialize_f64(self, v: f64) -> Result<Value, HashError> {
        float(v)
    }
    fn serialize_char(self, v: char) -> Result<Value, HashError> {
        Ok(Value::String(v.to_string()))
    }
    fn serialize_str(self, v: &str) -> Result<Value, HashError> {
        Ok(Value::String(v.to_owned()))
    }
    fn serialize_bytes(self, v: &[u8]) -> Result<Value, HashError> {
        Ok(Value::Array(v.iter().map(|b| Value::from(*b)).collect()))
    }
    fn serialize_none(self) -> Result<Value, HashError> {
        Ok(Value::Null)
    }
    fn serialize_some<T: Serialize + ?Sized>(self, v: &T) -> Result<Value, HashError> {
        v.serialize(Strict)
    }
    fn serialize_unit(self) -> Result<Value, HashError> {
        Ok(Value::Null)
    }
    fn serialize_unit_struct(self, _name: &'static str) -> Result<Value, HashError> {
        Ok(Value::Null)
    }
    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _idx: u32,
        variant: &'static str,
    ) -> Result<Value, HashError> {
        Ok(Value::String(variant.to_owned()))
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        v: &T,
    ) -> Result<Value, HashError> {
        v.serialize(Strict)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        _idx: u32,
        variant: &'static str,
        v: &T,
    ) -> Result<Value, HashError> {
        let mut m = Map::new();
        m.insert(variant.to_owned(), v.serialize(Strict)?);
        Ok(Value::Object(m))
    }
    fn serialize_seq(self, len: Option<usize>) -> Result<SeqS, HashError> {
        Ok(SeqS(Vec::with_capacity(len.unwrap_or(0))))
    }
    fn serialize_tuple(self, len: usize) -> Result<SeqS, HashError> {
        Ok(SeqS(Vec::with_capacity(len)))
    }
    fn serialize_tuple_struct(self, _name: &'static str, len: usize) -> Result<SeqS, HashError> {
        Ok(SeqS(Vec::with_capacity(len)))
    }
    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _idx: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<VariantSeqS, HashError> {
        Ok(VariantSeqS {
            variant,
            items: Vec::with_capacity(len),
        })
    }
    fn serialize_map(self, _len: Option<usize>) -> Result<MapS, HashError> {
        Ok(MapS {
            map: Map::new(),
            key: None,
        })
    }
    fn serialize_struct(self, _name: &'static str, _len: usize) -> Result<MapS, HashError> {
        Ok(MapS {
            map: Map::new(),
            key: None,
        })
    }
    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _idx: u32,
        variant: &'static str,
        _len: usize,
    ) -> Result<VariantMapS, HashError> {
        Ok(VariantMapS {
            variant,
            map: Map::new(),
        })
    }
}

struct SeqS(Vec<Value>);

impl ser::SerializeSeq for SeqS {
    type Ok = Value;
    type Error = HashError;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), HashError> {
        self.0.push(v.serialize(Strict)?);
        Ok(())
    }
    fn end(self) -> Result<Value, HashError> {
        Ok(Value::Array(self.0))
    }
}

impl ser::SerializeTuple for SeqS {
    type Ok = Value;
    type Error = HashError;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), HashError> {
        ser::SerializeSeq::serialize_element(self, v)
    }
    fn end(self) -> Result<Value, HashError> {
        ser::SerializeSeq::end(self)
    }
}

impl ser::SerializeTupleStruct for SeqS {
    type Ok = Value;
    type Error = HashError;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), HashError> {
        ser::SerializeSeq::serialize_element(self, v)
    }
    fn end(self) -> Result<Value, HashError> {
        ser::SerializeSeq::end(self)
    }
}

struct VariantSeqS {
    variant: &'static str,
    items: Vec<Value>,
}

impl ser::SerializeTupleVariant for VariantSeqS {
    type Ok = Value;
    type Error = HashError;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), HashError> {
        self.items.push(v.serialize(Strict)?);
        Ok(())
    }
    fn end(self) -> Result<Value, HashError> {
        let mut m = Map::new();
        m.insert(self.variant.to_owned(), Value::Array(self.items));
        Ok(Value::Object(m))
    }
}

struct MapS {
    map: Map<String, Value>,
    key: Option<String>,
}

impl ser::SerializeMap for MapS {
    type Ok = Value;
    type Error = HashError;
    fn serialize_key<T: Serialize + ?Sized>(&mut self, k: &T) -> Result<(), HashError> {
        self.key = Some(k.serialize(KeyS)?);
        Ok(())
    }
    fn serialize_value<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), HashError> {
        let key = self
            .key
            .take()
            .ok_or_else(|| HashError::NotCanonicalizable("value without key".to_owned()))?;
        self.map.insert(key, v.serialize(Strict)?);
        Ok(())
    }
    fn end(self) -> Result<Value, HashError> {
        Ok(Value::Object(self.map))
    }
}

impl ser::SerializeStruct for MapS {
    type Ok = Value;
    type Error = HashError;
    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        k: &'static str,
        v: &T,
    ) -> Result<(), HashError> {
        self.map.insert(k.to_owned(), v.serialize(Strict)?);
        Ok(())
    }
    fn end(self) -> Result<Value, HashError> {
        Ok(Value::Object(self.map))
    }
}

struct VariantMapS {
    variant: &'static str,
    map: Map<String, Value>,
}

impl ser::SerializeStructVariant for VariantMapS {
    type Ok = Value;
    type Error = HashError;
    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        k: &'static str,
        v: &T,
    ) -> Result<(), HashError> {
        self.map.insert(k.to_owned(), v.serialize(Strict)?);
        Ok(())
    }
    fn end(self) -> Result<Value, HashError> {
        let mut m = Map::new();
        m.insert(self.variant.to_owned(), Value::Object(self.map));
        Ok(Value::Object(m))
    }
}

/// Map keys must be strings (or newtypes / unit variants of strings).
struct KeyS;

fn non_string_key() -> HashError {
    HashError::NotCanonicalizable("map key is not a string".to_owned())
}

impl ser::Serializer for KeyS {
    type Ok = String;
    type Error = HashError;
    type SerializeSeq = ser::Impossible<String, HashError>;
    type SerializeTuple = ser::Impossible<String, HashError>;
    type SerializeTupleStruct = ser::Impossible<String, HashError>;
    type SerializeTupleVariant = ser::Impossible<String, HashError>;
    type SerializeMap = ser::Impossible<String, HashError>;
    type SerializeStruct = ser::Impossible<String, HashError>;
    type SerializeStructVariant = ser::Impossible<String, HashError>;

    fn serialize_bool(self, _v: bool) -> Result<String, HashError> {
        Err(non_string_key())
    }
    fn serialize_i8(self, _v: i8) -> Result<String, HashError> {
        Err(non_string_key())
    }
    fn serialize_i16(self, _v: i16) -> Result<String, HashError> {
        Err(non_string_key())
    }
    fn serialize_i32(self, _v: i32) -> Result<String, HashError> {
        Err(non_string_key())
    }
    fn serialize_i64(self, _v: i64) -> Result<String, HashError> {
        Err(non_string_key())
    }
    fn serialize_u8(self, _v: u8) -> Result<String, HashError> {
        Err(non_string_key())
    }
    fn serialize_u16(self, _v: u16) -> Result<String, HashError> {
        Err(non_string_key())
    }
    fn serialize_u32(self, _v: u32) -> Result<String, HashError> {
        Err(non_string_key())
    }
    fn serialize_u64(self, _v: u64) -> Result<String, HashError> {
        Err(non_string_key())
    }
    fn serialize_f32(self, _v: f32) -> Result<String, HashError> {
        Err(non_string_key())
    }
    fn serialize_f64(self, _v: f64) -> Result<String, HashError> {
        Err(non_string_key())
    }
    fn serialize_char(self, v: char) -> Result<String, HashError> {
        Ok(v.to_string())
    }
    fn serialize_str(self, v: &str) -> Result<String, HashError> {
        Ok(v.to_owned())
    }
    fn serialize_bytes(self, _v: &[u8]) -> Result<String, HashError> {
        Err(non_string_key())
    }
    fn serialize_none(self) -> Result<String, HashError> {
        Err(non_string_key())
    }
    fn serialize_some<T: Serialize + ?Sized>(self, v: &T) -> Result<String, HashError> {
        v.serialize(KeyS)
    }
    fn serialize_unit(self) -> Result<String, HashError> {
        Err(non_string_key())
    }
    fn serialize_unit_struct(self, _name: &'static str) -> Result<String, HashError> {
        Err(non_string_key())
    }
    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _idx: u32,
        variant: &'static str,
    ) -> Result<String, HashError> {
        Ok(variant.to_owned())
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        v: &T,
    ) -> Result<String, HashError> {
        v.serialize(KeyS)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        _idx: u32,
        _variant: &'static str,
        _v: &T,
    ) -> Result<String, HashError> {
        Err(non_string_key())
    }
    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq, HashError> {
        Err(non_string_key())
    }
    fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple, HashError> {
        Err(non_string_key())
    }
    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleStruct, HashError> {
        Err(non_string_key())
    }
    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _idx: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, HashError> {
        Err(non_string_key())
    }
    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, HashError> {
        Err(non_string_key())
    }
    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStruct, HashError> {
        Err(non_string_key())
    }
    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _idx: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, HashError> {
        Err(non_string_key())
    }
}
