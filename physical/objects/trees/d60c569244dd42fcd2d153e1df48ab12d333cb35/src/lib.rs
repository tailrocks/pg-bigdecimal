//! `ToSql` / `FromSql` bridge between `bigdecimal::BigDecimal` and
//! `PostgreSQL` `NUMERIC`.
//!
//! Adapted from <https://github.com/tzConnectBerlin/rust-pg_bigdecimal>
//! (MIT-licensed) and Diesel's `pg_numeric` module.
//!
//! `PostgreSQL` stores NUMERIC in a base-10000 representation with a
//! header containing the number of digits, weight (position of the
//! most-significant base-10000 digit), sign, and display scale.

use std::io::Cursor;

use bigdecimal::BigDecimal;
use byteorder::{BigEndian, ReadBytesExt};
use bytes::{BufMut, BytesMut};
use num_bigint::{BigInt, BigUint, Sign};
use num_integer::Integer;
use num_traits::{ToPrimitive, Zero};
use postgres_types::{FromSql, IsNull, ToSql, Type, to_sql_checked};

const SIGN_POS: u16 = 0x0000;
const SIGN_NEG: u16 = 0x4000;
const SIGN_NAN: u16 = 0xC000;

impl<'a> FromSql<'a> for PgNumeric {
    #[allow(clippy::cast_sign_loss)]
    fn from_sql(
        _ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        let mut cursor = Cursor::new(raw);
        let n_digits = cursor.read_u16::<BigEndian>()?;
        let weight = cursor.read_i16::<BigEndian>()?;
        let sign = cursor.read_u16::<BigEndian>()?;
        let scale = cursor.read_u16::<BigEndian>()?;

        if sign == SIGN_NAN {
            return Ok(PgNumeric(None));
        }

        let mut digits = Vec::with_capacity(n_digits as usize);
        for _ in 0..n_digits {
            digits.push(cursor.read_i16::<BigEndian>()?);
        }

        let mut value = BigUint::zero();
        let base = BigUint::from(10_000u32);
        for &d in &digits {
            value = value * &base + BigUint::from(d as u32);
        }

        let sign = if sign == SIGN_NEG {
            Sign::Minus
        } else {
            Sign::Plus
        };
        let bigint = BigInt::from_biguint(sign, value);
        let correction_exp = (i64::from(weight) - i64::from(n_digits) + 1) * 4;
        let bd = BigDecimal::new(bigint, -correction_exp).with_scale(i64::from(scale));
        Ok(PgNumeric(Some(bd)))
    }

    fn accepts(ty: &Type) -> bool {
        matches!(*ty, Type::NUMERIC)
    }
}

impl ToSql for PgNumeric {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap
    )]
    fn to_sql(
        &self,
        _ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        let Some(ref bd) = self.0 else {
            // NaN
            out.put_u16(0); // n_digits
            out.put_i16(0); // weight
            out.put_u16(SIGN_NAN);
            out.put_u16(0); // scale
            return Ok(IsNull::No);
        };

        if bd.is_zero() {
            out.put_u16(0);
            out.put_i16(0);
            out.put_u16(SIGN_POS);
            #[allow(clippy::cast_possible_truncation)]
            let (_, scale) = bd.as_bigint_and_exponent();
            out.put_u16(scale.max(0) as u16);
            return Ok(IsNull::No);
        }

        let (bigint, exponent) = bd.as_bigint_and_exponent();
        let sign = if bigint.sign() == Sign::Minus {
            SIGN_NEG
        } else {
            SIGN_POS
        };
        let magnitude = bigint.magnitude().clone();

        // Split into integer and fractional parts based on exponent.
        let scale = exponent.max(0);
        let (integer_part, fractional_part) = if exponent <= 0 {
            // No fractional part: value = bigint * 10^(-exponent)
            let factor = BigUint::from(10u32).pow((-exponent) as u32);
            (magnitude * factor, BigUint::zero())
        } else {
            let factor = BigUint::from(10u32).pow(exponent as u32);
            magnitude.div_rem(&factor)
        };

        // Convert integer part to base-10000 digits
        let mut integer_digits = to_base10000(&integer_part);
        let weight: i16 = if integer_digits.is_empty() {
            -1
        } else {
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            let w = (integer_digits.len() - 1) as i16;
            w
        };

        // Convert fractional part to base-10000 digits
        let mut frac_digits = Vec::new();
        if !fractional_part.is_zero() && scale > 0 {
            // Pad fractional part to fill complete base-10000 groups
            #[allow(clippy::cast_possible_truncation)]
            let scale_u = scale as u64;
            let groups = scale_u.div_ceil(4);
            let padded_digits = groups * 4;
            let mut frac = fractional_part;
            if padded_digits > scale_u {
                frac *= BigUint::from(10u32).pow((padded_digits - scale_u) as u32);
            }
            frac_digits = to_base10000(&frac);
            // Pad front to expected number of groups
            while frac_digits.len() < groups as usize {
                frac_digits.insert(0, 0);
            }
        }

        // Combine and strip trailing zeroes
        let mut all_digits = Vec::new();
        all_digits.append(&mut integer_digits);
        all_digits.append(&mut frac_digits);
        strip_trailing_zeroes(&mut all_digits);

        #[allow(clippy::cast_possible_truncation)]
        let n_digits = all_digits.len() as u16;

        out.put_u16(n_digits);
        out.put_i16(weight);
        out.put_u16(sign);
        out.put_u16(scale as u16);
        for d in &all_digits {
            out.put_i16(*d);
        }

        Ok(IsNull::No)
    }

    fn accepts(ty: &Type) -> bool {
        matches!(*ty, Type::NUMERIC)
    }

    to_sql_checked!();
}

/// Newtype wrapper to implement `ToSql`/`FromSql` for `BigDecimal`.
/// `None` represents `PostgreSQL` `NaN`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgNumeric(pub Option<BigDecimal>);

impl From<BigDecimal> for PgNumeric {
    fn from(bd: BigDecimal) -> Self {
        Self(Some(bd))
    }
}

impl From<Option<BigDecimal>> for PgNumeric {
    fn from(opt: Option<BigDecimal>) -> Self {
        Self(opt)
    }
}

/// Converts a `BigUint` to a vector of base-10000 digits (most significant first).
fn to_base10000(n: &BigUint) -> Vec<i16> {
    if n.is_zero() {
        return Vec::new();
    }
    let base = BigUint::from(10_000u32);
    let mut digits = Vec::new();
    let mut remaining = n.clone();
    while !remaining.is_zero() {
        let (quotient, remainder) = remaining.div_rem(&base);
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        digits.push(remainder.to_i16().unwrap_or(0));
        remaining = quotient;
    }
    digits.reverse();
    digits
}

fn strip_trailing_zeroes(digits: &mut Vec<i16>) {
    while digits.last() == Some(&0) {
        digits.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    /// Round-trip: `BigDecimal` → `PgNumeric` → bytes → `PgNumeric` → `BigDecimal`
    fn round_trip(input: &str) {
        let bd = BigDecimal::from_str(input).unwrap();
        let pg = PgNumeric(Some(bd.clone()));
        let mut buf = BytesMut::new();
        pg.to_sql(&Type::NUMERIC, &mut buf).unwrap();
        let back = PgNumeric::from_sql(&Type::NUMERIC, &buf).unwrap();
        let recovered = back.0.unwrap();
        // Compare with normalized scale
        assert_eq!(
            bd.normalized(),
            recovered.normalized(),
            "Round-trip failed for {input}"
        );
    }

    #[test]
    fn test_round_trip_positive() {
        round_trip("123456789.123456789");
    }

    #[test]
    fn test_round_trip_negative() {
        round_trip("-987654321.000001");
    }

    #[test]
    fn test_round_trip_zero() {
        round_trip("0");
    }

    #[test]
    fn test_round_trip_large() {
        round_trip("392908135046413249272161");
    }

    #[test]
    fn test_round_trip_very_large() {
        // uint256 max
        round_trip(
            "115792089237316195423570985008687907853269984665640564039457584007913129639935",
        );
    }

    #[test]
    fn test_round_trip_small_decimal() {
        round_trip("0.000000000000031337");
    }

    #[test]
    fn test_nan() {
        let pg = PgNumeric(None);
        let mut buf = BytesMut::new();
        pg.to_sql(&Type::NUMERIC, &mut buf).unwrap();
        let back = PgNumeric::from_sql(&Type::NUMERIC, &buf).unwrap();
        assert!(back.0.is_none());
    }

    #[test]
    fn test_integer() {
        round_trip("42");
        round_trip("10000");
        round_trip("99999999");
    }

    #[test]
    fn test_one() {
        round_trip("1");
        round_trip("1.0");
    }
}
