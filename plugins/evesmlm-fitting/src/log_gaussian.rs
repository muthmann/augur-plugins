use augur_plugin_evesmlm_candidates::EveCluster;
use nalgebra::{DMatrix, DVector};

use crate::FitEstimate;

pub(crate) fn fit(cluster: &EveCluster) -> Option<FitEstimate> {
    let samples: Vec<(f64, f64, f64)> = cluster
        .pixel_histogram
        .iter()
        .filter_map(|(x, y, positive, negative)| {
            let count = f64::from(*positive + *negative);
            if count <= 0.0 {
                None
            } else {
                Some((f64::from(*x), f64::from(*y), count))
            }
        })
        .collect();

    if samples.len() < 5 {
        return None;
    }

    let mut design = DMatrix::zeros(samples.len(), 5);
    let mut target = DVector::zeros(samples.len());
    for (row, (x, y, count)) in samples.iter().enumerate() {
        let weight = count.sqrt();
        design[(row, 0)] = weight;
        design[(row, 1)] = weight * *x;
        design[(row, 2)] = weight * *y;
        design[(row, 3)] = weight * x * x;
        design[(row, 4)] = weight * y * y;
        target[row] = weight * count.ln();
    }

    let lhs = design.transpose() * &design;
    let rhs = design.transpose() * target;
    let theta = lhs.lu().solve(&rhs)?;
    let a = theta[0];
    let b = theta[1];
    let c = theta[2];
    let d = theta[3];
    let e = theta[4];

    if d >= -1e-9 || e >= -1e-9 {
        return None;
    }

    let sigma_x = (-1.0 / (2.0 * d)).sqrt();
    let sigma_y = (-1.0 / (2.0 * e)).sqrt();
    let x = -b / (2.0 * d);
    let y = -c / (2.0 * e);
    if !a.is_finite()
        || !x.is_finite()
        || !y.is_finite()
        || !sigma_x.is_finite()
        || !sigma_y.is_finite()
    {
        return None;
    }

    let residual = (samples
        .iter()
        .map(|(sample_x, sample_y, count)| {
            let prediction =
                a + b * sample_x + c * sample_y + d * sample_x.powi(2) + e * sample_y.powi(2);
            let error = count.ln() - prediction;
            error * error
        })
        .sum::<f64>()
        / samples.len() as f64)
        .sqrt();

    Some(FitEstimate {
        x,
        y,
        sigma_x,
        sigma_y,
        residual,
    })
}
