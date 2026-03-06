use augur_plugin_evesmlm_candidates::EveCluster;

use crate::{mean_xy, FitEstimate};

pub(crate) fn fit(cluster: &EveCluster) -> Option<FitEstimate> {
    let samples: Vec<(f64, f64, f64)> = cluster
        .pixel_histogram
        .iter()
        .filter_map(|(x, y, positive, negative)| {
            let intensity = f64::from(*positive + *negative);
            if intensity <= 0.0 {
                None
            } else {
                Some((f64::from(*x), f64::from(*y), intensity))
            }
        })
        .collect();
    if samples.len() < 5 {
        return None;
    }

    let seed = mean_xy::fit(cluster)?;
    let (min_value, max_value) = samples.iter().fold(
        (f64::INFINITY, f64::NEG_INFINITY),
        |(min_value, max_value), (_, _, intensity)| {
            (min_value.min(*intensity), max_value.max(*intensity))
        },
    );

    let mut params = [
        seed.x,
        seed.y,
        ((f64::from(cluster.x_max - cluster.x_min) + 1.0) / 4.0).clamp(0.7, 4.0),
        ((f64::from(cluster.y_max - cluster.y_min) + 1.0) / 4.0).clamp(0.7, 4.0),
        (max_value - min_value).max(1.0),
        min_value.max(0.0),
    ];

    let mut best_error = f64::INFINITY;
    let mut damping = 1e-2;
    for _ in 0..20 {
        let (jtj, jtr, error) = gaussian_normal_equations(&samples, &params);
        if !error.is_finite() {
            return None;
        }

        let mut lhs = jtj;
        for (index, row) in lhs.iter_mut().enumerate() {
            row[index] += damping;
        }

        let delta = solve_linear_system(lhs, jtr)?;
        let candidate = [
            params[0] + delta[0],
            params[1] + delta[1],
            (params[2] + delta[2]).clamp(0.5, 6.0),
            (params[3] + delta[3]).clamp(0.5, 6.0),
            (params[4] + delta[4]).max(1e-3),
            (params[5] + delta[5]).max(0.0),
        ];
        let (_, _, candidate_error) = gaussian_normal_equations(&samples, &candidate);

        if candidate_error < error {
            params = candidate;
            best_error = candidate_error;
            damping *= 0.7;
        } else {
            damping *= 2.0;
        }

        let step_norm = delta.iter().map(|value| value * value).sum::<f64>().sqrt();
        if step_norm < 1e-3 {
            break;
        }
    }

    if !best_error.is_finite() {
        return None;
    }

    Some(FitEstimate {
        x: params[0],
        y: params[1],
        sigma_x: params[2],
        sigma_y: params[3],
        residual: (best_error / samples.len() as f64).sqrt(),
    })
}

fn gaussian_normal_equations(
    samples: &[(f64, f64, f64)],
    params: &[f64; 6],
) -> ([[f64; 6]; 6], [f64; 6], f64) {
    let mut jtj = [[0.0; 6]; 6];
    let mut jtr = [0.0; 6];
    let mut error = 0.0;

    let [x0, y0, sigma_x, sigma_y, amplitude, background] = *params;
    let sigma_x2 = sigma_x * sigma_x;
    let sigma_y2 = sigma_y * sigma_y;

    for (x, y, sample) in samples {
        let dx = *x - x0;
        let dy = *y - y0;
        let exponent = -0.5 * (dx * dx / sigma_x2 + dy * dy / sigma_y2);
        let gaussian = exponent.exp();
        let model = background + amplitude * gaussian;
        let residual = *sample - model;
        error += residual * residual;

        let jacobian = [
            amplitude * gaussian * (dx / sigma_x2),
            amplitude * gaussian * (dy / sigma_y2),
            amplitude * gaussian * (dx * dx / sigma_x.powi(3)),
            amplitude * gaussian * (dy * dy / sigma_y.powi(3)),
            gaussian,
            1.0,
        ];

        for row in 0..6 {
            jtr[row] += jacobian[row] * residual;
            for col in 0..6 {
                jtj[row][col] += jacobian[row] * jacobian[col];
            }
        }
    }

    (jtj, jtr, error)
}

fn solve_linear_system(mut lhs: [[f64; 6]; 6], mut rhs: [f64; 6]) -> Option<[f64; 6]> {
    for pivot in 0..6 {
        let mut best_row = pivot;
        let mut best_value = lhs[pivot][pivot].abs();
        for (row, lhs_row) in lhs.iter().enumerate().skip(pivot + 1) {
            let candidate = lhs_row[pivot].abs();
            if candidate > best_value {
                best_value = candidate;
                best_row = row;
            }
        }

        if best_value < 1e-9 {
            return None;
        }
        if best_row != pivot {
            lhs.swap(pivot, best_row);
            rhs.swap(pivot, best_row);
        }

        let pivot_value = lhs[pivot][pivot];
        for col in pivot..6 {
            lhs[pivot][col] /= pivot_value;
        }
        rhs[pivot] /= pivot_value;

        for row in 0..6 {
            if row == pivot {
                continue;
            }
            let factor = lhs[row][pivot];
            if factor.abs() < 1e-12 {
                continue;
            }
            for col in pivot..6 {
                lhs[row][col] -= factor * lhs[pivot][col];
            }
            rhs[row] -= factor * rhs[pivot];
        }
    }

    Some(rhs)
}
