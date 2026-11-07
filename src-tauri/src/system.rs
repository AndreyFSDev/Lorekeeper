//! Parameters of the whole system, not just the video card: CPU, RAM,
//! disk space. Needed both for settings selection (in CPU mode
//! the model lives in RAM, not VRAM) and in the interface.

use crate::paths;
use sysinfo::System;

/// `Default` — degradation for call sites (lib.rs): if the blocking task
/// `spawn_blocking(system::info)` failed, the interface gets an empty snapshot with
/// a boot.log entry instead of a panic.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SystemInfo {
    /// "AMD Ryzen 9 5950X · 16 cores / 32 threads".
    pub cpu: String,
    pub cpu_cores: usize,
    pub cpu_threads: usize,
    pub ram_total_mb: u64,
    pub ram_free_mb: u64,
    /// Free space in the model store and in the app folder (the smaller of the two).
    pub disk_free_gb: f64,
}

impl SystemInfo {
    pub fn ram_total_gb(&self) -> f64 {
        self.ram_total_mb as f64 / 1024.0
    }

    /// Whether there is enough RAM for a model of this size. We count by
    /// **total** memory, not free: free changes before one's eyes, and
    /// a recommendation depending on it would jump every time the screen opens.
    /// In CPU mode weights live in memory, so we leave half to
    /// the system, SillyTavern, and the KV cache. What fits in VRAM doesn't occupy RAM.
    pub fn fits_in_ram(&self, weights_bytes: u64, vram_mb: Option<u64>) -> bool {
        let weights_mb = weights_bytes / (1024 * 1024);
        let in_ram = weights_mb.saturating_sub(vram_mb.unwrap_or(0));
        self.ram_total_mb > 0 && in_ram * 2 <= self.ram_total_mb
    }
}

/// System snapshot. Must not be cached: free memory and disk space
/// change, and this is called only on interface request and during selection.
///
/// Blocking call (sysinfo + disk probes): call off the main thread —
/// Tauri sync commands run on the main thread, so call sites go
/// through `spawn_blocking` (lib.rs: `system_info`, `recommend_input`).
pub fn info() -> SystemInfo {
    let mut sys = System::new();
    sys.refresh_memory();
    sys.refresh_cpu();

    let cpu = sys
        .cpus()
        .first()
        .map(|c| c.brand().trim().to_string())
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "процессор не определён".into());
    let threads = sys.cpus().len();
    let cores = sys.physical_core_count().unwrap_or(threads);
    let cpu = format!(
        "{cpu} · {} / {}",
        plural(cores, "ядро", "ядра", "ядер"),
        plural(threads, "поток", "потока", "потоков")
    );

    let disk_free_gb = match (
        crate::gpu::free_space_bytes(&paths::models_dir()),
        crate::gpu::free_space_bytes(&paths::base_dir()),
    ) {
        (Some(models), Some(app)) => Some(models.min(app)),
        (models, app) => models.or(app),
    }
    .map(|b| b as f64 / 1024.0 / 1024.0 / 1024.0)
    .unwrap_or(-1.0);

    SystemInfo {
        cpu,
        cpu_cores: cores,
        cpu_threads: threads,
        ram_total_mb: sys.total_memory() / (1024 * 1024),
        ram_free_mb: sys.available_memory() / (1024 * 1024),
        disk_free_gb,
    }
}

fn plural(n: usize, one: &str, few: &str, many: &str) -> String {
    let last_two = n % 100;
    let last = n % 10;
    let word = if (11..=14).contains(&last_two) {
        many
    } else if last == 1 {
        one
    } else if (2..=4).contains(&last) {
        few
    } else {
        many
    };
    format!("{n} {word}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Memory for the model: what fit in VRAM doesn't occupy RAM.
    #[test]
    fn test_fits_in_ram_counts_only_the_part_outside_vram() {
        let sys = SystemInfo {
            cpu: "тест".into(),
            cpu_cores: 4,
            cpu_threads: 8,
            ram_total_mb: 16384,
            ram_free_mb: 4096,
            disk_free_gb: 100.0,
        };
        // 9.7 GB of weights with 12 GB of VRAM go entirely to the card.
        assert!(sys.fits_in_ram(10_400_000_000, Some(12000)));
        // The same file without a video card doesn't fit: 9.7 * 2 > 16 GB.
        assert!(!sys.fits_in_ram(10_400_000_000, None));
        // 5 GB on CPU — fits; free memory doesn't participate in the calculation.
        assert!(sys.fits_in_ram(5_400_000_000, None));
    }

    #[test]
    fn test_plural_ru() {
        assert_eq!(plural(1, "ядро", "ядра", "ядер"), "1 ядро");
        assert_eq!(plural(4, "ядро", "ядра", "ядер"), "4 ядра");
        assert_eq!(plural(11, "ядро", "ядра", "ядер"), "11 ядер");
        assert_eq!(plural(21, "поток", "потока", "потоков"), "21 поток");
    }

    /// Live snapshot: CPU and memory must be detected, otherwise
    /// the interface will show zeros and settings selection will lie.
    #[test]
    fn test_live_system_info_is_sane() {
        let sys = info();
        assert!(!sys.cpu.is_empty());
        assert!(sys.cpu_cores >= 1);
        assert!(sys.cpu_threads >= sys.cpu_cores);
        assert!(sys.ram_total_mb > 1024, "всего памяти: {} МБ", sys.ram_total_mb);
        assert!(sys.ram_free_mb > 0);
        println!(
            "CPU: {} | RAM: {} / {} МБ | диск: {:.1} ГБ",
            sys.cpu, sys.ram_free_mb, sys.ram_total_mb, sys.disk_free_gb
        );
    }
}
