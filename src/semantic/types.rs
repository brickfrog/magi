//! MAGI's value types.
//!
//! Every column carries a [`Type`] plus nullability ([`ColType`]). `Unknown` is used only for
//! columns whose schema cannot be known without contacting an external system (undeclared SQL
//! sources during a static `magi check`); expressions over `Unknown` are not type checked.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Type {
    Bool,
    Int,
    /// `decimal(precision, scale)`; precision is at most 38.
    Decimal(u8, u8),
    Float,
    String,
    Date,
    Time,
    Timestamp,
    TimestampTz,
    Binary,
    Json,
    /// The type of a bare `null` literal: compatible with everything.
    Null,
    /// Not known statically.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ColType {
    pub ty: Type,
    pub nullable: bool,
}

impl ColType {
    pub const fn new(ty: Type, nullable: bool) -> Self {
        Self { ty, nullable }
    }
    pub const fn nullable(ty: Type) -> Self {
        Self { ty, nullable: true }
    }
    pub const fn required(ty: Type) -> Self {
        Self {
            ty,
            nullable: false,
        }
    }
    pub fn with_nullable(self, nullable: bool) -> Self {
        Self { nullable, ..self }
    }
}

impl Type {
    pub fn is_numeric(self) -> bool {
        matches!(
            self,
            Type::Int | Type::Decimal(..) | Type::Float | Type::Null | Type::Unknown
        )
    }
    pub fn is_temporal(self) -> bool {
        matches!(self, Type::Date | Type::Timestamp | Type::TimestampTz)
    }
    pub fn is_unknown(self) -> bool {
        matches!(self, Type::Unknown)
    }

    /// DuckDB type used to store values of this type.
    pub fn duckdb_name(self) -> String {
        match self {
            Type::Bool => "BOOLEAN".into(),
            Type::Int => "BIGINT".into(),
            Type::Decimal(p, s) => format!("DECIMAL({p},{s})"),
            Type::Float => "DOUBLE".into(),
            Type::String | Type::Unknown | Type::Null => "VARCHAR".into(),
            Type::Date => "DATE".into(),
            Type::Time => "TIME".into(),
            Type::Timestamp => "TIMESTAMP".into(),
            Type::TimestampTz => "TIMESTAMPTZ".into(),
            Type::Binary => "BLOB".into(),
            Type::Json => "JSON".into(),
        }
    }

    /// Map a DuckDB column type name (as reported by `DESCRIBE`) to a MAGI type.
    pub fn from_duckdb(name: &str) -> Type {
        let upper = name.trim().to_ascii_uppercase();
        if let Some(inner) = upper
            .strip_prefix("DECIMAL(")
            .and_then(|s| s.strip_suffix(')'))
        {
            let mut parts = inner.split(',').map(|p| p.trim().parse::<u8>().ok());
            if let (Some(Some(p)), Some(Some(s))) = (parts.next(), parts.next()) {
                return Type::Decimal(p, s);
            }
        }
        match upper.as_str() {
            "BOOLEAN" | "BOOL" => Type::Bool,
            "TINYINT" | "SMALLINT" | "INTEGER" | "INT" | "BIGINT" | "UTINYINT" | "USMALLINT"
            | "UINTEGER" | "UBIGINT" | "HUGEINT" | "UHUGEINT" => Type::Int,
            "FLOAT" | "REAL" | "DOUBLE" => Type::Float,
            "VARCHAR" | "TEXT" | "STRING" | "UUID" => Type::String,
            "DATE" => Type::Date,
            "TIME" => Type::Time,
            "TIMESTAMP" | "TIMESTAMP_NS" | "TIMESTAMP_MS" | "TIMESTAMP_S" | "DATETIME" => {
                Type::Timestamp
            }
            "TIMESTAMP WITH TIME ZONE" | "TIMESTAMPTZ" => Type::TimestampTz,
            "BLOB" | "BYTEA" => Type::Binary,
            "JSON" => Type::Json,
            "\"NULL\"" | "NULL" => Type::Null,
            _ => Type::Unknown,
        }
    }

    /// The common supertype for values that flow into one column (union, coalesce, case).
    pub fn unify(a: Type, b: Type) -> Option<Type> {
        use Type::*;
        Some(match (a, b) {
            (x, y) if x == y => x,
            (Null, x) | (x, Null) => x,
            (Unknown, _) | (_, Unknown) => Unknown,
            (Int, Decimal(p, s)) | (Decimal(p, s), Int) => Decimal(p.max(18 + s).min(38), s),
            (Decimal(p1, s1), Decimal(p2, s2)) => {
                let s = s1.max(s2);
                let int_digits = (p1 - s1).max(p2 - s2);
                Decimal((int_digits + s).min(38), s)
            }
            (Float, x) | (x, Float) if x.is_numeric() => Float,
            (Date, Timestamp) | (Timestamp, Date) => Timestamp,
            _ => return None,
        })
    }

    /// Whether two types may be compared with `==`, `<`, etc.
    pub fn comparable(a: Type, b: Type) -> bool {
        (a.is_numeric() && b.is_numeric()) || Type::unify(a, b).is_some()
    }
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Type::Bool => f.write_str("bool"),
            Type::Int => f.write_str("int"),
            Type::Decimal(p, s) => write!(f, "decimal({p},{s})"),
            Type::Float => f.write_str("float"),
            Type::String => f.write_str("string"),
            Type::Date => f.write_str("date"),
            Type::Time => f.write_str("time"),
            Type::Timestamp => f.write_str("timestamp"),
            Type::TimestampTz => f.write_str("timestamp_tz"),
            Type::Binary => f.write_str("binary"),
            Type::Json => f.write_str("json"),
            Type::Null => f.write_str("null"),
            Type::Unknown => f.write_str("unknown"),
        }
    }
}

impl fmt::Display for ColType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.ty, if self.nullable { "?" } else { "" })
    }
}
