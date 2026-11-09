//! Calibration of a device model against real end-to-end measurements, with honest
//! cross-validation.
//!
//! # Fit
//! The `free` parameters are fitted in log-parameter space θ = ln(value), minimising
//! ```text
//!   Σ_i w_i · ln(sim_i / real_i)²  +  Σ_j ((θ_j − θ0_j) / σ)²
//! ```
//! where θ0 are the base model's values and σ = [`PRIOR_SIGMA`] (= ln 1000: a factor 1000 from
//! the base costs as much as one point off by a factor e, a factor 2 as much as one point 10 %
//! off), a weak prior so that a parameter the points barely constrain stays near its base value
//! instead of running off to absurd values, while real evidence moves it freely. A point with a measured range
//! `lo..hi` is weighted `w = 1 / (1 + (ln(hi/lo) / 2 / 0.1)²)`: a run-to-run spread of ±10 %
//! halves its weight. The optimizer is Nelder–Mead (deterministic: fixed initial simplex,
//! restarted from its best vertex until it stops improving). Simulation is deterministic:
//! FTL GC stalls are charged at their mean ([`simulate_plan`] with no seed).
//!
//! # Cross-validation
//! Points are dealt into k = min(n, [`MAX_FOLDS`]) folds (point i → fold i mod k; k = n is
//! leave-one-out). For each fold the model is re-fitted from the base on the other folds only,
//! and the fold's points are predicted with it: every "CV" number is a prediction for a point
//! the fit did not see (it cannot depend on the point's own `real`). Ranking accuracy on the
//! pairs uses these held-out predictions (and is also reported for the full fit).

use crate::sim::model::{Model, simulate_plan};
use crate::sim::replay::{Plan, Trace};
use anyhow::{Result, bail};

/// Prior width in ln units (see the module docs).
pub const PRIOR_SIGMA: f64 = 6.907_755_278_982_137; // ln 1000
/// At most this many cross-validation folds (n ≤ this: leave-one-out).
pub const MAX_FOLDS: usize = 10;
/// CV errors above this are flagged in the table.
pub const FLAG_ERR: f64 = 0.25;

/// One real measurement and the trace that reproduces it.
#[derive(Clone, Debug)]
pub struct CalPoint {
    pub id: String,
    pub drive: String,
    /// Measured seconds.
    pub real: f64,
    pub lo: Option<f64>,
    pub hi: Option<f64>,
    pub trace: Trace,
}

/// One point's row in the report.
#[derive(Clone, Debug, PartialEq)]
pub struct PointRow {
    pub id: String,
    pub drive: String,
    pub real: f64,
    pub lo: Option<f64>,
    pub hi: Option<f64>,
    /// Simulated with the base model, the full fit, and held out (CV).
    pub sim_base: f64,
    pub sim_full: f64,
    pub sim_cv: f64,
    /// Relative errors (sim / real − 1).
    pub err_full: f64,
    pub err_cv: f64,
    /// The model has no FTL and most of the point's writes are gapped (forward skips inside a
    /// 4 MiB unit, e.g. add's skipped cluster slack): its cost lies in AU merges, which only
    /// the FTL layer models.
    pub needs_ftl: bool,
}

impl PointRow {
    /// Whether `sim` lands inside the measured range (None without a range).
    pub fn in_range(&self, sim: f64) -> Option<bool> {
        match (self.lo, self.hi) {
            (Some(lo), Some(hi)) => Some(sim >= lo && sim <= hi),
            _ => None,
        }
    }
}

/// One ranking pair: `faster` measured faster than `slower`.
#[derive(Clone, Debug, PartialEq)]
pub struct PairRow {
    pub faster: String,
    pub slower: String,
    pub real_ok: bool,
    pub full_ok: bool,
    pub cv_ok: bool,
}

#[derive(Clone, Debug)]
pub struct CalReport {
    pub fitted: Model,
    pub base: Model,
    pub free: Vec<String>,
    pub folds: usize,
    pub points: Vec<PointRow>,
    pub pairs: Vec<PairRow>,
    /// Pairs naming a point that is not among the calibration points.
    pub skipped_pairs: Vec<(String, String)>,
    /// Objective of the full fit, and simulations run.
    pub objective: f64,
    pub evals: usize,
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 }
}

impl CalReport {
    pub fn fitted(&self) -> &Model {
        &self.fitted
    }

    /// (correct, total) ranking pairs on held-out predictions.
    pub fn ranking(&self) -> (usize, usize) {
        (self.pairs.iter().filter(|p| p.cv_ok).count(), self.pairs.len())
    }

    /// (correct, total) ranking pairs with the full fit.
    pub fn ranking_full(&self) -> (usize, usize) {
        (self.pairs.iter().filter(|p| p.full_ok).count(), self.pairs.len())
    }

    /// Median |CV error| (relative).
    pub fn median_abs_err_cv(&self) -> f64 {
        median(self.points.iter().map(|p| p.err_cv.abs()).collect())
    }

    /// Median |error| of the full fit (relative).
    pub fn median_abs_err_full(&self) -> f64 {
        median(self.points.iter().map(|p| p.err_full.abs()).collect())
    }

    /// Points whose held-out error exceeds [`FLAG_ERR`].
    pub fn flagged(&self) -> Vec<&PointRow> {
        self.points.iter().filter(|p| p.err_cv.abs() > FLAG_ERR).collect()
    }

    /// The report as markdown: summary, points, pairs, fitted parameters.
    pub fn table(&self) -> String {
        let pct = |e: f64| format!("{:+.1} %", e * 100.0);
        let rng = |p: &PointRow, s: f64| match p.in_range(s) {
            Some(true) => "yes",
            Some(false) => "no",
            None => "–",
        };
        let (cv_ok, n) = self.ranking();
        let (full_ok, _) = self.ranking_full();
        let mut s = format!(
            "**{}** · free: {} · {} points, {} folds{} · median |err| fit {:.1} %, held out {:.1} % · \
             ranking held out {cv_ok}/{n}, fit {full_ok}/{n}\n\n",
            self.fitted.name,
            if self.free.is_empty() { "none".to_string() } else { self.free.join(", ") },
            self.points.len(),
            self.folds,
            if self.folds == self.points.len() { " (leave-one-out)" } else { "" },
            self.median_abs_err_full() * 100.0,
            self.median_abs_err_cv() * 100.0,
        );
        s.push_str("| point | drive | real s | range | base s | fit s | err | held-out s | err | in range (fit / held-out) | flag |\n");
        s.push_str("|---|---|---:|---|---:|---:|---:|---:|---:|---|---|\n");
        for p in &self.points {
            let range = match (p.lo, p.hi) {
                (Some(lo), Some(hi)) => format!("{lo}–{hi}"),
                _ => "–".into(),
            };
            s.push_str(&format!(
                "| {} | {} | {} | {} | {:.3} | {:.3} | {} | {:.3} | {} | {} / {} | {} |\n",
                p.id,
                p.drive,
                p.real,
                range,
                p.sim_base,
                p.sim_full,
                pct(p.err_full),
                p.sim_cv,
                pct(p.err_cv),
                rng(p, p.sim_full),
                rng(p, p.sim_cv),
                match (p.err_cv.abs() > FLAG_ERR, p.needs_ftl) {
                    (true, true) => "**> 25 %**, gapped writes: stick history / GC not in the trace",
                    (true, false) => "**> 25 %**",
                    (false, true) => "",
                    _ => "",
                },
            ));
        }
        if !self.pairs.is_empty() {
            s.push_str("\n| faster | slower | real | fit | held out |\n|---|---|---|---|---|\n");
            let ok = |b: bool| if b { "ok" } else { "**WRONG**" };
            for p in &self.pairs {
                s.push_str(&format!(
                    "| {} | {} | {} | {} | {} |\n",
                    p.faster,
                    p.slower,
                    ok(p.real_ok),
                    ok(p.full_ok),
                    ok(p.cv_ok)
                ));
            }
        }
        for (a, b) in &self.skipped_pairs {
            s.push_str(&format!("\n(pair {a} < {b} skipped: not among the points)"));
        }
        if !self.free.is_empty() {
            s.push_str("\n| parameter | base | fitted | × |\n|---|---:|---:|---:|\n");
            for k in &self.free {
                let (b, f) = (self.base.get(k).unwrap_or(0.0), self.fitted.get(k).unwrap_or(0.0));
                s.push_str(&format!("| {k} | {b:.6} | {f:.6} | {:.3} |\n", if b > 0.0 { f / b } else { 0.0 }));
            }
        }
        s
    }
}

struct Problem<'a> {
    plans: &'a [Plan],
    reals: Vec<f64>,
    weights: Vec<f64>,
    base: &'a Model,
    free: &'a [&'a str],
    theta0: Vec<f64>,
}

impl Problem<'_> {
    fn model(&self, theta: &[f64]) -> Model {
        let mut m = self.base.clone();
        for (k, t) in self.free.iter().zip(theta) {
            // set only fails on non-finite / out-of-range values: keep the base value then
            let _ = m.set(k, t.clamp(-700.0, 700.0).exp());
        }
        m
    }

    fn objective(&self, theta: &[f64], idx: &[usize], evals: &mut usize) -> f64 {
        let m = self.model(theta);
        let sims = sim_all(self.plans, idx, &m);
        *evals += idx.len();
        let mut f = 0.0;
        for (&i, s) in idx.iter().zip(&sims) {
            let r = (s.max(1e-12) / self.reals[i]).ln();
            f += self.weights[i] * r * r;
        }
        for (t, t0) in theta.iter().zip(&self.theta0) {
            let z = (t - t0) / PRIOR_SIGMA;
            f += z * z;
        }
        f
    }
}

/// Simulates the plans `idx` on `m` (in parallel threads when there are several).
fn sim_all(plans: &[Plan], idx: &[usize], m: &Model) -> Vec<f64> {
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(idx.len()).max(1);
    if threads == 1 {
        return idx.iter().map(|&i| simulate_plan(&plans[i], m, None).secs).collect();
    }
    let mut out = vec![0.0; idx.len()];
    std::thread::scope(|s| {
        let chunk = idx.len().div_ceil(threads);
        for (ids, o) in idx.chunks(chunk).zip(out.chunks_mut(chunk)) {
            s.spawn(move || {
                for (i, slot) in ids.iter().zip(o.iter_mut()) {
                    *slot = simulate_plan(&plans[*i], m, None).secs;
                }
            });
        }
    });
    out
}

/// Whether most writes (at least 100) skip forward by less than 4 MiB from the previous write's
/// end: gapped streams whose real cost is in AU merges.
fn gapped_writes(t: &Trace) -> bool {
    let (mut n, mut gapped, mut last) = (0u64, 0u64, None::<u64>);
    for e in &t.evs {
        if let crate::sim::replay::Ev::Io { write: true, off, len, .. } = *e {
            n += 1;
            if last.is_some_and(|l| off > l && off - l < 4 << 20) {
                gapped += 1;
            }
            last = Some(off + len);
        }
    }
    n >= 100 && gapped * 2 > n
}

/// Nelder–Mead from `x0` (deterministic). Returns (best x, best f).
fn nelder_mead(f: &mut dyn FnMut(&[f64]) -> f64, x0: &[f64], step: f64, max_evals: usize) -> (Vec<f64>, f64) {
    let d = x0.len();
    let mut used = 0usize;
    let mut best = (x0.to_vec(), f(x0));
    used += 1;
    if d == 0 {
        return best;
    }
    loop {
        // (re)start a simplex around the best point
        let mut s: Vec<(Vec<f64>, f64)> = vec![best.clone()];
        for j in 0..d {
            let mut x = best.0.clone();
            x[j] += step;
            let fx = f(&x);
            used += 1;
            s.push((x, fx));
        }
        let start_f = best.1;
        while used < max_evals {
            s.sort_by(|a, b| a.1.total_cmp(&b.1));
            let spread = s[d].1 - s[0].1;
            let size = s
                .iter()
                .skip(1)
                .map(|v| v.0.iter().zip(&s[0].0).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max))
                .fold(0.0, f64::max);
            if spread <= 1e-10 * (1.0 + s[0].1.abs()) && size < 1e-4 {
                break;
            }
            if size < 1e-7 {
                break;
            }
            let mut c = vec![0.0; d];
            for v in &s[..d] {
                for (cj, xj) in c.iter_mut().zip(&v.0) {
                    *cj += xj / d as f64;
                }
            }
            let at = |t: f64| -> Vec<f64> { c.iter().zip(&s[d].0).map(|(cj, wj)| cj + t * (wj - cj)).collect() };
            let xr = at(-1.0);
            let fr = f(&xr);
            used += 1;
            if fr < s[0].1 {
                let xe = at(-2.0);
                let fe = f(&xe);
                used += 1;
                s[d] = if fe < fr { (xe, fe) } else { (xr, fr) };
            } else if fr < s[d - 1].1 {
                s[d] = (xr, fr);
            } else {
                let (xc, fc) = if fr < s[d].1 {
                    let x = at(-0.5);
                    let fx = f(&x);
                    (x, fx)
                } else {
                    let x = at(0.5);
                    let fx = f(&x);
                    (x, fx)
                };
                used += 1;
                if fc < s[d].1.min(fr) {
                    s[d] = (xc, fc);
                } else {
                    let x0 = s[0].0.clone();
                    for v in s.iter_mut().skip(1) {
                        for (vj, bj) in v.0.iter_mut().zip(&x0) {
                            *vj = bj + 0.5 * (*vj - bj);
                        }
                        v.1 = f(&v.0);
                        used += 1;
                    }
                }
            }
        }
        s.sort_by(|a, b| a.1.total_cmp(&b.1));
        if s[0].1 < best.1 {
            best = s[0].clone();
        }
        // restart only while a restart still improves meaningfully
        if used >= max_evals || start_f - best.1 <= 1e-9 * (1.0 + best.1.abs()) {
            return best;
        }
    }
}

/// Fits `free` parameters of `base` to `points` (see the module docs) and cross-validates.
/// All points are simulated on the same model: calibrate one drive at a time.
pub fn calibrate(points: &[CalPoint], pairs: &[(String, String)], base: &Model, free: &[&str]) -> Result<CalReport> {
    if points.is_empty() {
        bail!("calibration needs at least one point");
    }
    for p in points {
        if !(p.real.is_finite() && p.real > 0.0) {
            bail!("point {}: real time must be > 0", p.id);
        }
    }
    let mut theta0 = Vec::new();
    for k in free {
        match base.get(k) {
            Some(v) if v > 0.0 => theta0.push(v.ln()),
            Some(_) => bail!("free parameter {k} is 0 in the base model: log-space calibration needs a positive start"),
            None if Model::keys().contains(k) => bail!("free parameter {k} is not set in the base model (no FTL?)"),
            None => bail!("unknown model parameter {k:?}"),
        }
    }
    let plans: Vec<Plan> = points.iter().map(|p| p.trace.plan()).collect();
    let weights = points
        .iter()
        .map(|p| match (p.lo, p.hi) {
            (Some(lo), Some(hi)) if lo > 0.0 && hi > lo => {
                let s = (hi / lo).ln() / 2.0 / 0.1;
                1.0 / (1.0 + s * s)
            }
            _ => 1.0,
        })
        .collect();
    let prob = Problem { plans: &plans, reals: points.iter().map(|p| p.real).collect(), weights, base, free, theta0 };
    let n = points.len();
    let all: Vec<usize> = (0..n).collect();
    let max_evals = 300 + 150 * free.len();
    let mut evals = 0usize;
    let fit = |idx: &[usize], evals: &mut usize| -> (Vec<f64>, f64) {
        let mut obj = |th: &[f64]| prob.objective(th, idx, evals);
        nelder_mead(&mut obj, &prob.theta0, 0.5, max_evals)
    };
    let (theta_full, objective) = fit(&all, &mut evals);
    let fitted = {
        let mut m = prob.model(&theta_full);
        m.name = base.name.clone();
        m
    };
    let folds = n.min(MAX_FOLDS);
    let mut sim_cv = vec![0.0; n];
    for f in 0..folds {
        let train: Vec<usize> = (0..n).filter(|i| i % folds != f).collect();
        let test: Vec<usize> = (0..n).filter(|i| i % folds == f).collect();
        let m = if train.is_empty() || free.is_empty() {
            base.clone()
        } else {
            let (th, _) = fit(&train, &mut evals);
            prob.model(&th)
        };
        for (i, s) in test.iter().zip(sim_all(&plans, &test, &m)) {
            sim_cv[*i] = s;
        }
    }
    let sim_full = sim_all(&plans, &all, &fitted);
    let sim_base = sim_all(&plans, &all, base);
    let rows: Vec<PointRow> = points
        .iter()
        .enumerate()
        .map(|(i, p)| PointRow {
            id: p.id.clone(),
            drive: p.drive.clone(),
            real: p.real,
            lo: p.lo,
            hi: p.hi,
            sim_base: sim_base[i],
            sim_full: sim_full[i],
            sim_cv: sim_cv[i],
            err_full: sim_full[i] / p.real - 1.0,
            err_cv: sim_cv[i] / p.real - 1.0,
            needs_ftl: base.ftl.is_none() && gapped_writes(&p.trace),
        })
        .collect();
    let mut prs = Vec::new();
    let mut skipped = Vec::new();
    for (a, b) in pairs {
        match (rows.iter().find(|r| &r.id == a), rows.iter().find(|r| &r.id == b)) {
            (Some(x), Some(y)) => prs.push(PairRow {
                faster: a.clone(),
                slower: b.clone(),
                real_ok: x.real < y.real,
                full_ok: x.sim_full < y.sim_full,
                cv_ok: x.sim_cv < y.sim_cv,
            }),
            _ => skipped.push((a.clone(), b.clone())),
        }
    }
    Ok(CalReport {
        fitted,
        base: base.clone(),
        free: free.iter().map(|s| s.to_string()).collect(),
        folds,
        points: rows,
        pairs: prs,
        skipped_pairs: skipped,
        objective,
        evals,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::model::simulate_det;
    use crate::sim::profiles;

    fn synth(truth: &Model) -> Vec<CalPoint> {
        let specs = [
            ("w4k", "seqw", 4096u64, 1usize, 4u64 << 20),
            ("w64k", "seqw", 65536, 1, 8 << 20),
            ("w1m", "seqw", 1 << 20, 1, 16 << 20),
            ("w8m", "seqw", 8 << 20, 1, 32 << 20),
            ("r4k", "seqr", 4096, 1, 4 << 20),
            ("r1m", "seqr", 1 << 20, 1, 16 << 20),
            ("r8m2", "seqr", 8 << 20, 2, 32 << 20),
        ];
        specs
            .iter()
            .map(|&(id, kind, bs, qd, bytes)| {
                let trace = Trace::micro(kind, bs, qd, bytes).unwrap();
                let real = simulate_det(&trace, truth).secs;
                CalPoint { id: id.into(), drive: "synthetic".into(), real, lo: None, hi: None, trace }
            })
            .collect()
    }

    fn base() -> Model {
        let mut m = profiles::sandisk();
        m.ftl = None;
        m
    }

    #[test]
    fn recovers_known_parameters() {
        let mut truth = base();
        truth.wr.bw = 5.2e6;
        truth.wr.cmd = 0.0014;
        truth.rd.bw = 17e6;
        truth.rd.cmd = 0.00028;
        let pts = synth(&truth);
        let free = ["wr.bw", "wr.cmd", "rd.bw", "rd.cmd"];
        let rep = calibrate(&pts, &[("w4k".into(), "w8m".into())], &base(), &free).unwrap();
        let f = rep.fitted();
        for k in free {
            let (a, b) = (f.get(k).unwrap(), truth.get(k).unwrap());
            // the weak prior toward the base costs a few percent on exact synthetic data
            assert!((a / b - 1.0).abs() < 0.05, "{k}: fitted {a} vs truth {b}");
        }
        assert!(rep.median_abs_err_full() < 0.01, "{}", rep.median_abs_err_full());
        assert!(rep.median_abs_err_cv() < 0.05, "{}", rep.median_abs_err_cv());
        assert_eq!(rep.ranking(), (1, 1));
        assert_eq!(rep.folds, pts.len());
        let t = rep.table();
        assert!(t.contains("| w4k |") && t.contains("wr.bw"), "{t}");
    }

    #[test]
    fn held_out_prediction_ignores_own_real() {
        let mut truth = base();
        truth.wr.bw = 5e6;
        let pts = synth(&truth);
        let free = ["wr.bw", "wr.cmd"];
        let a = calibrate(&pts, &[], &base(), &free).unwrap();
        let mut pts2 = pts.clone();
        pts2[0].real *= 3.0;
        let b = calibrate(&pts2, &[], &base(), &free).unwrap();
        assert_eq!(a.points[0].sim_cv, b.points[0].sim_cv);
        // ...while the full fit, which saw it, moves
        assert_ne!(a.points[0].sim_full, b.points[0].sim_full);
        // and other points' held-out predictions (whose fits saw point 0) move too
        assert_ne!(a.points[1].sim_cv, b.points[1].sim_cv);
    }

    #[test]
    fn flags_ranges_and_errors() {
        let truth = base();
        let mut pts = synth(&truth);
        pts[0].real *= 2.0;
        pts[1].lo = Some(pts[1].real * 0.9);
        pts[1].hi = Some(pts[1].real * 1.1);
        let rep = calibrate(&pts, &[("w4k".into(), "w8m".into()), ("x".into(), "y".into())], &base(), &[]).unwrap();
        assert!(rep.flagged().iter().any(|p| p.id == "w4k"));
        assert_eq!(rep.points[1].in_range(rep.points[1].sim_cv), Some(true));
        assert_eq!(rep.skipped_pairs.len(), 1);
        assert!(rep.table().contains("**> 25 %**"));
        assert!(calibrate(&pts, &[], &base(), &["rd.qpen"]).is_err());
        assert!(calibrate(&pts, &[], &base(), &["nope"]).is_err());
        assert!(calibrate(&[], &[], &base(), &[]).is_err());
        // gapped writes on a model without FTL are marked for the FTL layer
        let g = Trace {
            evs: (0..200u64)
                .map(|i| crate::sim::replay::Ev::Io {
                    write: true,
                    off: i * 32768,
                    len: 4096,
                    qd: 1,
                    grp: 0,
                    seq: 0,
                    t: 0,
                    d: 0,
                })
                .collect(),
            ..Trace::default()
        };
        let gp = CalPoint { id: "g".into(), drive: "s".into(), real: 1.0, lo: None, hi: None, trace: g };
        let r = calibrate(std::slice::from_ref(&gp), &[], &base(), &[]).unwrap();
        assert!(r.points[0].needs_ftl);
        assert!(!calibrate(&[gp], &[], &profiles::sandisk(), &[]).unwrap().points[0].needs_ftl);
    }
}

#[cfg(test)]
/// Developer harness: `SIMDIR=<dir of <id-without-drive-prefix>.jsonl traces> [SIMDRIVE=sandisk]
/// [SIMFREE=wr.bw,...] [SIMMODEL=spec] cargo test --release --lib dev_calibrate -- --ignored
/// --nocapture` calibrates against scenarios.txt and prints the table.
mod dev {
    use super::*;

    #[test]
    #[ignore]
    fn dev_calibrate() {
        let dir = std::env::var("SIMDIR").unwrap();
        let drive = std::env::var("SIMDRIVE").unwrap_or("sandisk".into());
        let free_s = std::env::var("SIMFREE").unwrap_or_default();
        let free: Vec<&str> = free_s.split(',').filter(|s| !s.is_empty()).collect();
        let base = match std::env::var("SIMMODEL") {
            Ok(p) => crate::sim::profiles::resolve(&p).unwrap(),
            Err(_) => crate::sim::profiles::builtin(&drive).unwrap(),
        };
        let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/sim/scenarios.txt")).unwrap();
        let mut pts = Vec::new();
        let mut pairs = Vec::new();
        for l in text.lines() {
            let l = l.trim();
            if l.starts_with('#') || l.is_empty() {
                continue;
            }
            let kv: std::collections::HashMap<&str, &str> =
                l.split_whitespace().filter_map(|w| w.split_once('=')).collect();
            if l.starts_with("pair ") {
                pairs.push((kv["faster"].to_string(), kv["slower"].to_string()));
                continue;
            }
            if kv["drive"] != drive {
                continue;
            }
            let id = kv["id"];
            let trace = if kv["op"] == "micro" {
                Trace::micro(
                    kv["kind"],
                    kv["bs"].parse().unwrap(),
                    kv["qd"].parse().unwrap(),
                    kv["bytes"].parse().unwrap(),
                )
                .unwrap()
            } else {
                let f = format!("{dir}/{}.jsonl", id.split_once('-').unwrap().1);
                match Trace::load(std::path::Path::new(&f)) {
                    Ok(t) => t,
                    Err(_) => continue,
                }
            };
            let num = |k: &str| kv.get(k).map(|v| v.parse::<f64>().unwrap());
            pts.push(CalPoint {
                id: id.into(),
                drive: drive.clone(),
                real: num("real").unwrap(),
                lo: num("lo"),
                hi: num("hi"),
                trace,
            });
        }
        let t0 = std::time::Instant::now();
        let rep = calibrate(&pts, &pairs, &base, &free).unwrap();
        println!("{}\nevals {} in {:?}\n{}", rep.table(), rep.evals, t0.elapsed(), rep.fitted().render());
    }
}
