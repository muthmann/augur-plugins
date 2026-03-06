use augur_core::pipeline::CdEvent;
use nalgebra::Matrix2;

pub fn filter_clusters(
    events: &[CdEvent],
    clusters: Vec<Vec<usize>>,
    max_spatial_extent_px: f64,
    min_isotropy: f64,
) -> Vec<Vec<usize>> {
    let max_variance = max_spatial_extent_px.max(0.0).powi(2);

    clusters
        .into_iter()
        .filter(|indices| {
            let Some((lambda_1, lambda_2)) = cluster_eigenvalues(events, indices) else {
                return false;
            };
            let isotropy = if lambda_1 <= 1e-9 {
                1.0
            } else {
                lambda_2 / lambda_1
            };
            lambda_1 <= max_variance && isotropy >= min_isotropy
        })
        .collect()
}

pub fn cluster_eigenvalues(events: &[CdEvent], indices: &[usize]) -> Option<(f64, f64)> {
    if indices.len() < 2 {
        return None;
    }

    let n = indices.len() as f64;
    let mean_x = indices
        .iter()
        .map(|&index| f64::from(events[index].x))
        .sum::<f64>()
        / n;
    let mean_y = indices
        .iter()
        .map(|&index| f64::from(events[index].y))
        .sum::<f64>()
        / n;

    let mut covariance = Matrix2::zeros();
    for &index in indices {
        let dx = f64::from(events[index].x) - mean_x;
        let dy = f64::from(events[index].y) - mean_y;
        covariance[(0, 0)] += dx * dx;
        covariance[(0, 1)] += dx * dy;
        covariance[(1, 0)] += dx * dy;
        covariance[(1, 1)] += dy * dy;
    }
    covariance /= n.max(1.0);

    let eigen = covariance.symmetric_eigen();
    let mut eigenvalues = [eigen.eigenvalues[0], eigen.eigenvalues[1]];
    eigenvalues.sort_by(|left, right| right.total_cmp(left));
    Some((eigenvalues[0], eigenvalues[1]))
}
