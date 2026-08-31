use ::usearch::MetricKind;

use crate::error::{Error, Result};
use crate::handler::quant::Quant;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Metric {
    Cos,
    L2,
    IP,
    Hamming,
    Tanimoto,
}

impl Metric {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "cos" | "cosine" => Some(Self::Cos),
            "l2" | "l2sq" | "euclidean" => Some(Self::L2),
            "ip" | "dot" => Some(Self::IP),
            "hamming" => Some(Self::Hamming),
            "tanimoto" | "jaccard" => Some(Self::Tanimoto),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cos => "cos",
            Self::L2 => "l2",
            Self::IP => "ip",
            Self::Hamming => "hamming",
            Self::Tanimoto => "tanimoto",
        }
    }

    pub(super) const fn kind(self) -> MetricKind {
        match self {
            Self::Cos => MetricKind::Cos,
            Self::L2 => MetricKind::L2sq,
            Self::IP => MetricKind::IP,
            Self::Hamming => MetricKind::Hamming,
            Self::Tanimoto => MetricKind::Tanimoto,
        }
    }

    /// The similarity a caller sees, from the distance usearch reports.
    ///
    /// All scores are calculated as `1.0 - distance` according to the new design,
    /// where L2 uses the euclidean distance (`sqrt` of the squared distance reported by usearch).
    pub fn score(self, distance: f32, _dim: usize) -> f32 {
        match self {
            Self::L2 => 1.0 - distance.max(0.0).sqrt(),
            _ => 1.0 - distance,
        }
    }

    pub fn check_quant(self, quant: Quant) -> Result<()> {
        let bitwise = matches!(self, Self::Hamming | Self::Tanimoto);
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

        assert!(Metric::Cos.check_quant(Quant::B1).is_err());

        assert!(Metric::Hamming.check_quant(Quant::F32).is_err());
    }

    #[test]
    fn identical_vectors_score_one() {
        assert!((Metric::Cos.score(0.0, 4) - 1.0).abs() < 1e-6);
        assert!((Metric::IP.score(0.0, 4) - 1.0).abs() < 1e-6);
        assert!((Metric::Tanimoto.score(0.0, 4) - 1.0).abs() < 1e-6);
        assert!((Metric::L2.score(0.0, 4) - 1.0).abs() < 1e-6);
        assert!((Metric::Hamming.score(0.0, 16) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn l2_undoes_the_squaring() {
        // (0,0) to (3,4): usearch reports 25, the euclidean distance is 5
        // 1 - 5 = -4
        assert!((Metric::L2.score(25.0, 2) + 4.0).abs() < 1e-6);
    }

    #[test]
    fn score_decreases_as_distance_grows() {
        for metric in [
            Metric::Cos,
            Metric::L2,
            Metric::IP,
            Metric::Hamming,
            Metric::Tanimoto,
        ] {
            let near = metric.score(1.0, 64);
            let far = metric.score(4.0, 64);
            assert!(
                near > far,
                "{metric} ranked a farther vector at least as high: {near} vs {far}"
            );
        }
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
