use std::f64::consts::TAU;

use augur_plugin_evesmlm_candidates::EveCluster;
use num_complex::Complex64;

use crate::{mean_xy, FitEstimate};

pub(crate) fn fit(cluster: &EveCluster) -> Option<FitEstimate> {
    let width = usize::from(cluster.x_max - cluster.x_min) + 1;
    let height = usize::from(cluster.y_max - cluster.y_min) + 1;
    if width < 2 || height < 2 {
        return mean_xy::fit(cluster);
    }

    let mut fx = Complex64::new(0.0, 0.0);
    let mut fy = Complex64::new(0.0, 0.0);
    let mut total = 0.0;

    for (x, y, positive, negative) in &cluster.pixel_histogram {
        let intensity = f64::from(*positive + *negative);
        if intensity <= 0.0 {
            continue;
        }
        total += intensity;
        let local_x = f64::from(*x - cluster.x_min);
        let local_y = f64::from(*y - cluster.y_min);
        fx += Complex64::from_polar(intensity, -TAU * local_x / width as f64);
        fy += Complex64::from_polar(intensity, -TAU * local_y / height as f64);
    }

    if total <= 0.0 {
        return None;
    }

    let x_local = (-fx.arg() * width as f64 / TAU).rem_euclid(width as f64);
    let y_local = (-fy.arg() * height as f64 / TAU).rem_euclid(height as f64);
    let ratio_x = (fx.norm() / total).clamp(0.0, 1.0);
    let ratio_y = (fy.norm() / total).clamp(0.0, 1.0);

    Some(FitEstimate {
        x: f64::from(cluster.x_min) + x_local,
        y: f64::from(cluster.y_min) + y_local,
        sigma_x: 0.0,
        sigma_y: 0.0,
        residual: 1.0 - 0.5 * (ratio_x + ratio_y),
    })
}
