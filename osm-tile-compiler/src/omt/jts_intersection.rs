//! Double-double line intersection used by pinned JTS 1.20.
//! Adapted from JTS CGAlgorithmsDD.intersection and DD arithmetic.
//! Copyright (c) 2016 Vivid Solutions. SPDX-License-Identifier: EDL-1.0.
//! See LICENSE.jts-edl-1.0.txt. Used by the buffer and union port in `jts_buffer`.

#[derive(Clone, Copy)]
struct Dd {
    hi: f64,
    lo: f64,
}
impl Dd {
    fn new(hi: f64) -> Self {
        Self { hi, lo: 0. }
    }
    fn add(self, y: Self) -> Self {
        let sum = self.hi + y.hi;
        let tail = self.lo + y.lo;
        let e = sum - self.hi;
        let f = tail - self.lo;
        let s = sum - e;
        let t = tail - f;
        let s = (y.hi - e) + (self.hi - s);
        let t = (y.lo - f) + (self.lo - t);
        let e = s + tail;
        let h_big = sum + e;
        let h_small = e + (sum - h_big);
        let e = t + h_small;
        let hi = h_big + e;
        Self {
            hi,
            lo: e + (h_big - hi),
        }
    }
    fn sub(self, y: Self) -> Self {
        self.add(Self {
            hi: -y.hi,
            lo: -y.lo,
        })
    }
    fn mul(self, y: Self) -> Self {
        const SPLIT: f64 = 134_217_729.;
        let big = SPLIT * self.hi;
        let hx = big - self.hi;
        let c = SPLIT * y.hi;
        let hx = big - hx;
        let tx = self.hi - hx;
        let hy = c - y.hi;
        let product = self.hi * y.hi;
        let hy = c - hy;
        let ty = y.hi - hy;
        let correction = ((((hx * hy - product) + hx * ty) + tx * hy) + tx * ty)
            + (self.hi * y.lo + self.lo * y.hi);
        let hi = product + correction;
        Self {
            hi,
            lo: correction + (product - hi),
        }
    }
    fn div(self, y: Self) -> Self {
        const SPLIT: f64 = 134_217_729.;
        let quotient = self.hi / y.hi;
        let c = SPLIT * quotient;
        let hc = c - quotient;
        let u = SPLIT * y.hi;
        let hc = c - hc;
        let tc = quotient - hc;
        let hy = u - y.hi;
        let product = quotient * y.hi;
        let hy = u - hy;
        let ty = y.hi - hy;
        let error = (((hc * hy - product) + hc * ty) + tc * hy) + tc * ty;
        let correction = ((((self.hi - product) - error) + self.lo) - quotient * y.lo) / y.hi;
        let hi = quotient + correction;
        Self {
            hi,
            lo: (quotient - hi) + correction,
        }
    }
    fn value(self) -> f64 {
        self.hi + self.lo
    }
}

/// Intersection of infinite lines. Segment classification, endpoint copying,
/// and out-of-envelope fallback are the noder's responsibility.
pub(super) fn intersection(
    p1: [f64; 2],
    p2: [f64; 2],
    q1: [f64; 2],
    q2: [f64; 2],
) -> Option<[f64; 2]> {
    let d = Dd::new;
    let px = d(p1[1]).sub(d(p2[1]));
    let py = d(p2[0]).sub(d(p1[0]));
    let pw = d(p1[0]).mul(d(p2[1])).sub(d(p2[0]).mul(d(p1[1])));
    let qx = d(q1[1]).sub(d(q2[1]));
    let qy = d(q2[0]).sub(d(q1[0]));
    let qw = d(q1[0]).mul(d(q2[1])).sub(d(q2[0]).mul(d(q1[1])));
    let x = py.mul(qw).sub(qy.mul(pw));
    let y = qx.mul(pw).sub(px.mul(qw));
    let w = px.mul(qy).sub(qx.mul(py));
    let point = [x.div(w).value(), y.div(w).value()];
    point.iter().all(|x| x.is_finite()).then_some(point)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_buffer_intersections_match_pinned_double_double_bits() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/omt-jts-intersections.json"
        ))
        .unwrap();
        let point = |v: &serde_json::Value| {
            [0, 1].map(|i| f64::from_bits(u64::from_str_radix(v[i].as_str().unwrap(), 16).unwrap()))
        };
        for (index, case) in fixture["cases"].as_array().unwrap().iter().enumerate() {
            let actual = intersection(
                point(&case["a"][0]),
                point(&case["a"][1]),
                point(&case["b"][0]),
                point(&case["b"][1]),
            )
            .unwrap();
            let expected = point(&case["dd"]);
            assert_eq!(
                actual.map(f64::to_bits),
                expected.map(f64::to_bits),
                "case {index}"
            );
        }
    }
    #[test]
    fn proper_and_parallel_lines() {
        assert_eq!(
            intersection([0., 0.], [2., 2.], [0., 2.], [2., 0.]),
            Some([1., 1.])
        );
        assert_eq!(intersection([0., 0.], [2., 2.], [0., 1.], [2., 3.]), None);
    }
}
