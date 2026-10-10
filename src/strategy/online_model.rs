//! Rolling ridge model. A signal's label is available only at its scheduled exit open.
use crate::data::StockSymbol;
use std::collections::{BTreeMap, VecDeque};
const N: usize = 16;
pub(super) type Factors = [f64; N];
struct Sample {
    symbol: StockSymbol,
    x: Factors,
    entry: Option<f64>,
}
struct Pending {
    day: usize,
    samples: Vec<Sample>,
}
struct Moments {
    day: usize,
    xx: [[f64; N]; N],
    xy: Factors,
    count: usize,
}
pub(super) struct OnlineModel {
    horizon: usize,
    window: usize,
    ridge: f64,
    pending: VecDeque<Pending>,
    history: VecDeque<Moments>,
    xx: [[f64; N]; N],
    xy: Factors,
    count: usize,
    coefficients: Option<Factors>,
}
impl OnlineModel {
    pub fn new(horizon: usize, window: usize, ridge: f64) -> Self {
        Self {
            horizon,
            window,
            ridge,
            pending: VecDeque::new(),
            history: VecDeque::new(),
            xx: [[0.0; N]; N],
            xy: [0.0; N],
            count: 0,
            coefficients: None,
        }
    }
    pub fn symbols(&self) -> impl Iterator<Item = StockSymbol> + '_ {
        self.pending
            .iter()
            .flat_map(|p| p.samples.iter().map(|s| s.symbol))
    }
    pub fn enqueue(&mut self, day: usize, rows: Vec<(StockSymbol, Factors)>) {
        self.pending.push_back(Pending {
            day,
            samples: rows
                .into_iter()
                .map(|(symbol, x)| Sample {
                    symbol,
                    x,
                    entry: None,
                })
                .collect(),
        });
    }
    /// Open prices are factor-adjusted observations from this date, never future bars.
    pub fn observe(&mut self, day: usize, opens: &BTreeMap<StockSymbol, f64>) {
        for batch in self.pending.iter_mut().filter(|batch| day == batch.day + 1) {
            for sample in &mut batch.samples {
                sample.entry = opens.get(&sample.symbol).copied();
            }
        }
        while self
            .pending
            .front()
            .is_some_and(|batch| day > batch.day + self.horizon)
        {
            let batch = self.pending.pop_front().unwrap();
            let mut moment = Moments {
                day,
                xx: [[0.0; N]; N],
                xy: [0.0; N],
                count: 0,
            };
            // Missing scheduled opens invalidate the label; no stale-price substitution.
            for sample in batch.samples {
                let Some(entry) = sample.entry else { continue };
                let Some(&exit) = opens.get(&sample.symbol) else {
                    continue;
                };
                let y = (exit / entry - 1.0).clamp(-0.5, 0.5) - 0.0011;
                moment.count += 1;
                for (i, &xi) in sample.x.iter().enumerate() {
                    moment.xy[i] += xi * y;
                    for (j, &xj) in sample.x.iter().enumerate() {
                        moment.xx[i][j] += xi * xj;
                    }
                }
            }
            self.add(&moment, 1.0);
            self.history.push_back(moment);
        }
        while self
            .history
            .front()
            .is_some_and(|m| day - m.day >= self.window)
        {
            let old = self.history.pop_front().unwrap();
            self.add(&old, -1.0);
        }
        self.coefficients = if self.count >= 500 { self.fit() } else { None };
    }
    fn add(&mut self, m: &Moments, sign: f64) {
        if sign > 0.0 {
            self.count += m.count;
        } else {
            self.count -= m.count;
        }
        for (i, xy) in self.xy.iter_mut().enumerate() {
            *xy += sign * m.xy[i];
            for (j, xx) in self.xx[i].iter_mut().enumerate() {
                *xx += sign * m.xx[i][j];
            }
        }
    }
    fn fit(&self) -> Option<Factors> {
        let mut a = self.xx;
        let mut b = self.xy;
        for (i, row) in a.iter_mut().enumerate() {
            row[i] += if i == 0 {
                1e-8
            } else {
                self.ridge * self.count as f64
            };
        }
        for col in 0..N {
            let pivot = (col..N).max_by(|&i, &j| a[i][col].abs().total_cmp(&a[j][col].abs()))?;
            if a[pivot][col].abs() < 1e-10 {
                return None;
            }
            a.swap(col, pivot);
            b.swap(col, pivot);
            let divisor = a[col][col];
            for v in &mut a[col][col..] {
                *v /= divisor;
            }
            b[col] /= divisor;
            let row = a[col];
            let value = b[col];
            for i in 0..N {
                if i == col {
                    continue;
                }
                let ratio = a[i][col];
                for (j, aj) in a[i].iter_mut().enumerate().skip(col) {
                    *aj -= ratio * row[j];
                }
                b[i] -= ratio * value;
            }
        }
        b.iter().all(|v| v.is_finite()).then_some(b)
    }
    pub fn predict(&self, x: Factors) -> Option<f64> {
        self.coefficients
            .map(|w| w.iter().zip(x).map(|(w, x)| w * x).sum())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn labels_mature_at_exit_open_and_old_observations_expire() {
        let symbol = StockSymbol::from("000001");
        let mut x = [0.0; N];
        x[0] = 1.0;
        let mut m = OnlineModel::new(2, 3, 0.1);
        m.enqueue(1, vec![(symbol, x); 500]);
        m.observe(2, &BTreeMap::from([(symbol, 10.0)]));
        m.observe(3, &BTreeMap::from([(symbol, 999.0)]));
        assert!(m.predict(x).is_none());
        m.observe(4, &BTreeMap::from([(symbol, 11.0)]));
        assert!((m.predict(x).unwrap() - 0.0989).abs() < 1e-8);
        m.observe(7, &BTreeMap::new());
        assert!(m.predict(x).is_none());
    }
    #[test]
    fn missing_entry_or_exit_does_not_create_a_label() {
        let s = StockSymbol::from("000001");
        let mut x = [0.0; N];
        x[0] = 1.0;
        let mut m = OnlineModel::new(2, 3, 0.1);
        m.enqueue(1, vec![(s, x); 500]);
        m.observe(2, &BTreeMap::new());
        m.observe(4, &BTreeMap::from([(s, 11.0)]));
        assert_eq!(m.count, 0);
        m.enqueue(5, vec![(s, x); 500]);
        m.observe(6, &BTreeMap::from([(s, 10.0)]));
        m.observe(8, &BTreeMap::new());
        assert_eq!(m.count, 0);
    }
}
