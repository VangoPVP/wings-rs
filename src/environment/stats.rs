use bollard::models::ContainerStatsResponse;

pub fn cpu_absolute(s: &ContainerStatsResponse) -> f64 {
    let (Some(cur), Some(pre)) = (&s.cpu_stats, &s.precpu_stats) else {
        return 0.0;
    };
    let total = |c: &bollard::models::ContainerCpuStats| {
        c.cpu_usage.as_ref().and_then(|u| u.total_usage).unwrap_or(0)
    };
    let cpu_delta = total(cur) as f64 - total(pre) as f64;
    let sys_delta = cur.system_cpu_usage.unwrap_or(0) as f64 - pre.system_cpu_usage.unwrap_or(0) as f64;
    let cpus = cur.online_cpus.map(|n| n as f64).unwrap_or_else(|| {
        cur.cpu_usage
            .as_ref()
            .and_then(|u| u.percpu_usage.as_ref())
            .map(|v| v.len() as f64)
            .unwrap_or(1.0)
    });
    if cpu_delta <= 0.0 || sys_delta <= 0.0 {
        return 0.0;
    }
    let pct = (cpu_delta / sys_delta) * cpus * 100.0;
    (pct * 1000.0).round() / 1000.0
}

pub fn memory_usage(s: &ContainerStatsResponse) -> u64 {
    let Some(m) = &s.memory_stats else { return 0 };
    let usage = m.usage.unwrap_or(0);
    if let Some(stats) = &m.stats {
        for key in ["total_inactive_file", "inactive_file"] {
            if let Some(v) = stats.get(key)
                && *v < usage
            {
                return usage - v;
            }
        }
    }
    usage
}

pub fn memory_limit(s: &ContainerStatsResponse) -> u64 {
    s.memory_stats.as_ref().and_then(|m| m.limit).unwrap_or(0)
}

pub fn network(s: &ContainerStatsResponse) -> (u64, u64) {
    s.networks
        .as_ref()
        .map(|n| {
            n.values().fold((0, 0), |(rx, tx), v| {
                (rx + v.rx_bytes.unwrap_or(0), tx + v.tx_bytes.unwrap_or(0))
            })
        })
        .unwrap_or((0, 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bollard::models::{ContainerCpuStats, ContainerCpuUsage, ContainerMemoryStats};
    use std::collections::HashMap;

    fn cpu(total: u64, sys: u64) -> ContainerCpuStats {
        ContainerCpuStats {
            cpu_usage: Some(ContainerCpuUsage {
                total_usage: Some(total),
                ..Default::default()
            }),
            system_cpu_usage: Some(sys),
            online_cpus: Some(4),
            ..Default::default()
        }
    }

    #[test]
    fn cpu_calc_matches_docker() {
        let s = ContainerStatsResponse {
            cpu_stats: Some(cpu(2_000, 10_000)),
            precpu_stats: Some(cpu(1_000, 6_000)),
            ..Default::default()
        };
        assert_eq!(cpu_absolute(&s), 100.0);
    }

    #[test]
    fn memory_excludes_cache() {
        let s = ContainerStatsResponse {
            memory_stats: Some(ContainerMemoryStats {
                usage: Some(1000),
                limit: Some(4000),
                stats: Some(HashMap::from([("inactive_file".to_string(), 300)])),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(memory_usage(&s), 700);
        assert_eq!(memory_limit(&s), 4000);
    }
}
