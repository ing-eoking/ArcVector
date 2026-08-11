//! Distance metrics, and which quantizations each one is meaningful on.
//!
//! The pairing is not free: `b1` packs one bit per dimension and so carries no
//! magnitude, which leaves only the bitwise metrics; and those metrics have
//! nothing to compare on anything else. `vcreate` refuses the combinations that
//! would produce distances with no meaning.

use ::usearch::MetricKind;

use crate::arcus::element::Quant;
use crate::error::{Error, Result};

/// Distance metric. Restricted per quantization by [`Metric::check_quant`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Metric {
    Cos,
    L2,
    IP,
    Hamming,
    Tanimoto,
}

impl Metric {
    pub fn parse(s: &str) -> Option<Metric> {
        match s.to_ascii_lowercase().as_str() {
            "cos" | "cosine" => Some(Metric::Cos),
            "l2" | "l2sq" | "euclidean" => Some(Metric::L2),
            "ip" | "dot" => Some(Metric::IP),
            "hamming" => Some(Metric::Hamming),
            "tanimoto" | "jaccard" => Some(Metric::Tanimoto),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Metric::Cos => "cos",
            Metric::L2 => "l2",
            Metric::IP => "ip",
            Metric::Hamming => "hamming",
            Metric::Tanimoto => "tanimoto",
        }
    }

    pub(super) const fn kind(self) -> MetricKind {
        match self {
            Metric::Cos => MetricKind::Cos,
            Metric::L2 => MetricKind::L2sq,
            Metric::IP => MetricKind::IP,
            Metric::Hamming => MetricKind::Hamming,
            Metric::Tanimoto => MetricKind::Tanimoto,
        }
    }

    /// Reject metric/quantization pairs whose distances would be meaningless.
    pub fn check_quant(self, quant: Quant) -> Result<()> {
        let bitwise = matches!(self, Metric::Hamming | Metric::Tanimoto);
        match (quant, bitwise) {
            (Quant::B1, false) => Err(Error::bad_request(format!(
                "quantization b1 requires a bitwise metric (hamming or tanimoto), got {self}"
            ))),
            (q, true) if q != Quant::B1 => Err(Error::bad_request(format!(
                "metric {self} requires quantization b1, got {q}"
            ))),
            _ => Ok(()),
        }
    }
}

impl std::fmt::Display for Metric {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_quant_compatibility() {
        assert!(Metric::Cos.check_quant(Quant::I8).is_ok());
        assert!(Metric::L2.check_quant(Quant::F32).is_ok());
        assert!(Metric::Hamming.check_quant(Quant::B1).is_ok());
        assert!(Metric::Tanimoto.check_quant(Quant::B1).is_ok());

        // b1 vectors carry no magnitude, so cosine/L2/IP are meaningless.
        assert!(Metric::Cos.check_quant(Quant::B1).is_err());
        // ...and bitwise metrics are meaningless on non-bit vectors.
        assert!(Metric::Hamming.check_quant(Quant::F32).is_err());
    }

    #[test]
    fn metric_names_roundtrip() {
        for m in [
            Metric::Cos,
            Metric::L2,
            Metric::IP,
            Metric::Hamming,
            Metric::Tanimoto,
        ] {
            assert_eq!(Metric::parse(m.as_str()), Some(m));
        }
        assert_eq!(Metric::parse("cosine"), Some(Metric::Cos));
        assert_eq!(Metric::parse("nonsense"), None);
    }
}
