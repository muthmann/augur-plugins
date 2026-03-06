use augur_plugin_evesmlm_candidates::EveCluster;

use crate::FitEstimate;

pub(crate) fn fit(cluster: &EveCluster) -> Option<FitEstimate> {
    let mut weight_sum = 0.0;
    let mut weighted_x = 0.0;
    let mut weighted_y = 0.0;

    for (x, y, positive, negative) in &cluster.pixel_histogram {
        let weight = f64::from(*positive + *negative);
        if weight <= 0.0 {
            continue;
        }
        weight_sum += weight;
        weighted_x += f64::from(*x) * weight;
        weighted_y += f64::from(*y) * weight;
    }

    if weight_sum <= 0.0 {
        return None;
    }

    Some(FitEstimate {
        x: weighted_x / weight_sum,
        y: weighted_y / weight_sum,
        sigma_x: 0.0,
        sigma_y: 0.0,
        residual: 0.0,
    })
}
