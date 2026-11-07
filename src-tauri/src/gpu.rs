use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GpuVendor {
    Nvidia,
    Amd,
    Intel,
    Unknown,
}

/// What can actually accelerate inference on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuBackend {
    /// CUDA: Ollama and ComfyUI work as is.
    Cuda,
    /// Vulkan: an AMD or Intel card accelerated by the Ollama Vulkan backend
    /// (`lib/ollama/vulkan` is part of our ollama-windows-amd64.zip package).
    /// No images here — ComfyUI only supports CUDA.
    Vulkan,
    /// No acceleration: the CPU runs the models.
    Cpu,
}

/// Hardware tier: the model catalog is filtered by it so the user sees
/// only what their machine can really handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HardwareTier {
    /// No discrete card or a card without acceleration: light models only.
    Minimal,
    /// 7–10.9 GB VRAM: 8B chat, SD 1.5 for images.
    Low,
    /// 11–14.9 GB VRAM: 14B chat in Q4, SDXL.
    Mid,
    /// 15 GB VRAM and more: 14B with headroom, SDXL without limits.
    High,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GpuInfo {
    pub has_gpu: bool,
    pub vendor: GpuVendor,
    pub name: Option<String>,
    pub vram_mb: Option<u64>,
    pub backend: GpuBackend,
    pub tier: HardwareTier,
    /// Explanations for the UI: why exactly this model showcase was chosen.
    pub notes: Vec<String>,
}

use std::sync::OnceLock;

static CACHED_GPU: OnceLock<GpuInfo> = OnceLock::new();

/// GPU info: name, VRAM size, what accelerates inference.
/// Cached: detection spawns child processes and calls DXGI.
pub fn gpu_info() -> GpuInfo {
    CACHED_GPU.get_or_init(detect_gpu).clone()
}

const TIER_HIGH_VRAM_MB: u64 = 15000;
const TIER_MID_VRAM_MB: u64 = 11000;
const TIER_LOW_VRAM_MB: u64 = 7000;

pub fn tier_for(vram_mb: Option<u64>, backend: GpuBackend) -> HardwareTier {
    match backend {
        GpuBackend::Cpu => HardwareTier::Minimal,
        GpuBackend::Cuda | GpuBackend::Vulkan => match vram_mb.unwrap_or(0) {
            v if v >= TIER_HIGH_VRAM_MB => HardwareTier::High,
            v if v >= TIER_MID_VRAM_MB => HardwareTier::Mid,
            v if v >= TIER_LOW_VRAM_MB => HardwareTier::Low,
            _ => HardwareTier::Minimal,
        },
    }
}

/// GPU detection: `nvidia-smi` first (exact VRAM size and live
/// CUDA), then DXGI when it is silent, and the CPU at the very end.
/// The probe order and its weak spot: on a laptop with switchable graphics
/// a powered-off dGPU does not answer nvidia-smi and is not listed by DXGI, and both
/// probes honestly report the remaining hardware (iGPU or `Cpu`) — the tier drops without
/// the card changing. The value lives in `OnceLock` until the process ends, so protection
/// against such a miss lives in tuning: `recommend::auto_tune_plan` does not touch
/// the settings on a suspicious tier drop (V-58).
fn detect_gpu() -> GpuInfo {
    if let Some(info) = detect_nvidia() {
        return info;
    }
    if let Some(mut info) = detect_dxgi() {
        // nvidia-smi is silent while DXGI sees NVIDIA: the card is present, but CUDA
        // may be unavailable (powered-off dGPU, driver failure) — we say so
        // directly instead of reporting a quiet tier as with a healthy card.
        if info.vendor == GpuVendor::Nvidia {
            info.notes.push(
                "nvidia-smi не ответил — CUDA может быть недоступна (обесточенный dGPU ноутбука или сбой драйвера).".into(),
            );
        }
        return info;
    }
    GpuInfo {
        has_gpu: false,
        vendor: GpuVendor::Unknown,
        name: None,
        vram_mb: None,
        backend: GpuBackend::Cpu,
        tier: HardwareTier::Minimal,
        notes: vec!["Дискретная видеокарта не найдена — модели считает процессор.".into()],
    }
}

/// NVIDIA: nvidia-smi gives the exact VRAM size and confirms that CUDA is available.
fn detect_nvidia() -> Option<GpuInfo> {
    let mut cmd = Command::new("nvidia-smi");
    cmd.args(["--query-gpu=name,memory.total", "--format=csv,noheader,nounits"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // nvidia-smi lines come in bus order, not by power: in a "GT 710 + RTX 4070" pair the first is
    // the weak card. The largest one runs the models.
    let (name, vram_mb) = text
        .lines()
        .map(|line| {
            let parts: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
            let name = parts.first().filter(|s| !s.is_empty()).map(|s| s.to_string());
            (name, parts.get(1).and_then(|s| s.parse::<u64>().ok()))
        })
        .max_by_key(|(_, vram)| vram.unwrap_or(0))?;
    Some(GpuInfo {
        has_gpu: true,
        vendor: GpuVendor::Nvidia,
        name,
        vram_mb,
        backend: GpuBackend::Cuda,
        tier: tier_for(vram_mb, GpuBackend::Cuda),
        notes: Vec::new(),
    })
}

/// Free VRAM and a flag that this is a real measurement, not an estimate.
/// A measurement exists only on NVIDIA (`nvidia-smi`); on Vulkan and on the NVIDIA fallback
/// it returns a share of the total size that the loaded model already occupies —
/// adding it to the budget a second time is not allowed.
pub fn free_vram() -> Option<(u64, bool)> {
    let info = gpu_info();
    if info.backend == GpuBackend::Cpu {
        return None;
    }
    if info.vendor == GpuVendor::Nvidia {
        if let Some(free) = nvidia_free_vram_mb() {
            return Some((free, true));
        }
    }
    info.vram_mb.map(|total| (total * 4 / 5, false))
}

fn nvidia_free_vram_mb() -> Option<u64> {
    // The indicator poll and settings save call this from async chains, while nvidia-smi on
    // a busy card takes seconds: a short cache avoids spawning a process on every call
    static CACHE: std::sync::Mutex<Option<(std::time::Instant, Option<u64>)>> = std::sync::Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, free)) = *cache {
        if at.elapsed() < std::time::Duration::from_secs(2) {
            return free;
        }
    }
    let free = query_nvidia_free_vram_mb();
    *cache = Some((std::time::Instant::now(), free));
    free
}

fn query_nvidia_free_vram_mb() -> Option<u64> {
    let mut cmd = Command::new("nvidia-smi");
    cmd.args(["--query-gpu=memory.total,memory.free", "--format=csv,noheader,nounits"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    // Free memory of the same card that detect_nvidia picked: the largest one
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let (total, free) = line.split_once(',')?;
            Some((total.trim().parse::<u64>().ok()?, free.trim().parse::<u64>().ok()?))
        })
        .max_by_key(|(total, _)| *total)
        .map(|(_, free)| free)
}

/// AMD/Intel: nvidia-smi does not see them, so we ask DXGI.
/// WMI (`Win32_VideoController.AdapterRAM`) is not suitable for this — the field is 32-bit
/// and truncates the size at 4 GB, so we take DedicatedVideoMemory from the adapter description.
#[cfg(windows)]
fn detect_dxgi() -> Option<GpuInfo> {
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE,
    };

    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1().ok()? };
    let mut best: Option<GpuInfo> = None;

    for index in 0..8u32 {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(index) }) else {
            break;
        };
        let Ok(desc) = (unsafe { adapter.GetDesc1() }) else {
            continue;
        };
        if (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0 {
            continue;
        }
        let name_len = desc.Description.iter().position(|c| *c == 0).unwrap_or(desc.Description.len());
        let name = String::from_utf16_lossy(&desc.Description[..name_len]).trim().to_string();
        let vram_mb = desc.DedicatedVideoMemory as u64 / (1024 * 1024);
        let vendor = match desc.VendorId {
            0x10DE => GpuVendor::Nvidia,
            0x1002 | 0x1022 => GpuVendor::Amd,
            0x8086 => GpuVendor::Intel,
            _ => GpuVendor::Unknown,
        };
        if vram_mb < 512 {
            continue;
        }
        let candidate = build_vendor_info(vendor, name, vram_mb);
        let better = match &best {
            None => true,
            Some(current) => candidate.vram_mb.unwrap_or(0) > current.vram_mb.unwrap_or(0),
        };
        if better {
            best = Some(candidate);
        }
    }

    best
}

/// Card profile by vendor and size: which models to show it and what it runs on.
pub fn build_vendor_info(vendor: GpuVendor, name: String, vram_mb: u64) -> GpuInfo {
    let mut notes = Vec::new();
    let backend = match vendor {
        GpuVendor::Nvidia => GpuBackend::Cuda,
        GpuVendor::Amd => {
            // ROCm is not part of the program bundle and does not support Polaris/Vega,
            // so the only available acceleration on AMD is the Ollama Vulkan backend.
            if is_legacy_amd(&name) {
                notes.push(
                    "Карта AMD поколения Polaris/Vega: ROCm её не поддерживает, ускорение идёт через Vulkan-бэкенд Ollama.".into(),
                );
            } else {
                notes.push(
                    "Карта AMD: ускорение идёт через Vulkan-бэкенд Ollama — ROCm в комплект программы не входит.".into(),
                );
            }
            notes.push(
                "Генерация картинок на AMD недоступна: ComfyUI работает только на CUDA.".into(),
            );
            GpuBackend::Vulkan
        }
        GpuVendor::Intel => {
            notes.push(
                "Intel: ускорение возможно через Vulkan-бэкенд Ollama, но видеопамять общая с системной.".into(),
            );
            GpuBackend::Vulkan
        }
        GpuVendor::Unknown => GpuBackend::Cpu,
    };
    let tier = tier_for(Some(vram_mb), backend);
    GpuInfo {
        has_gpu: true,
        vendor,
        name: Some(name),
        vram_mb: Some(vram_mb),
        backend,
        tier,
        notes,
    }
}

/// RX 400/500 and Vega: gfx803/gfx900 are outside current ROCm support.
fn is_legacy_amd(name: &str) -> bool {
    let lower = name.to_lowercase();
    if lower.contains("rx 4") || lower.contains("rx 5") || lower.contains("vega") {
        return true;
    }
    // The Radeon RX 500 series is sometimes reported as "Radeon (TM) RX 580".
    lower.contains("polaris")
}

#[cfg(not(windows))]
fn detect_dxgi() -> Option<GpuInfo> {
    None
}

/// Free disk space at the given path, bytes.
#[cfg(windows)]
pub fn free_space_bytes(path: &Path) -> Option<u64> {
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    use windows_core::PCWSTR;

    let mut cur = path.to_path_buf();
    while !cur.exists() {
        if let Some(parent) = cur.parent() {
            cur = parent.to_path_buf();
        } else {
            break;
        }
    }
    let canonical = cur.canonicalize().unwrap_or(cur);
    let s = canonical.to_string_lossy();
    let clean = s.strip_prefix(r"\\?\").unwrap_or(&s);
    let mut path_str = clean.to_string();
    if !path_str.ends_with('\\') && !path_str.ends_with('/') {
        path_str.push('\\');
    }
    let wide: Vec<u16> = path_str.encode_utf16().chain(std::iter::once(0)).collect();
    let mut free = 0u64;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(wide.as_ptr()),
            Some(&mut free),
            None,
            None,
        )
    };
    if ok.is_err() {
        None
    } else {
        Some(free)
    }
}

#[cfg(not(windows))]
pub fn free_space_bytes(_path: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_free_space_bytes() {
        let path = std::env::current_dir().unwrap();
        let free = free_space_bytes(&path);
        assert!(free.is_some());
        assert!(free.unwrap() > 0);

        let non_existent = path.join("subfolder_does_not_exist_yet");
        let free_nested = free_space_bytes(&non_existent);
        assert!(free_nested.is_some());
        assert!(free_nested.unwrap() > 0);
    }

    #[test]
    fn test_tier_thresholds() {
        assert_eq!(tier_for(Some(16303), GpuBackend::Cuda), HardwareTier::High);
        assert_eq!(tier_for(Some(15000), GpuBackend::Cuda), HardwareTier::High);
        assert_eq!(tier_for(Some(12288), GpuBackend::Cuda), HardwareTier::Mid);
        assert_eq!(tier_for(Some(8192), GpuBackend::Cuda), HardwareTier::Low);
        assert_eq!(tier_for(Some(4096), GpuBackend::Cuda), HardwareTier::Minimal);
        assert_eq!(tier_for(None, GpuBackend::Cpu), HardwareTier::Minimal);
        // Even a large card without acceleration counts as the minimal tier:
        // in Ollama it computes on the CPU.
        assert_eq!(tier_for(Some(16384), GpuBackend::Cpu), HardwareTier::Minimal);
    }

    #[test]
    fn test_amd_polaris_gets_vulkan_and_notes() {
        let info = build_vendor_info(GpuVendor::Amd, "Radeon RX 580 Series".into(), 8192);
        assert_eq!(info.vendor, GpuVendor::Amd);
        assert_eq!(info.backend, GpuBackend::Vulkan);
        assert_eq!(info.tier, HardwareTier::Low);
        assert!(!info.notes.is_empty(), "пользователю нужно объяснить, почему нет картинок");
    }

    #[test]
    fn test_amd_modern_gets_vulkan() {
        let info = build_vendor_info(GpuVendor::Amd, "AMD Radeon RX 7800 XT".into(), 16384);
        assert_eq!(info.backend, GpuBackend::Vulkan);
        assert_eq!(info.tier, HardwareTier::High);
    }

    #[test]
    fn test_intel_and_nvidia_backends() {
        let intel = build_vendor_info(GpuVendor::Intel, "Intel(R) Arc(TM) A750 Graphics".into(), 8192);
        assert_eq!(intel.backend, GpuBackend::Vulkan);
        assert_eq!(intel.tier, HardwareTier::Low);

        // Integrated graphics: DXGI reports only a few megabytes of dedicated memory.
        let igpu = build_vendor_info(GpuVendor::Intel, "Intel(R) UHD Graphics 630".into(), 128);
        assert_eq!(igpu.tier, HardwareTier::Minimal);

        let nvidia = build_vendor_info(GpuVendor::Nvidia, "NVIDIA GeForce RTX 3060".into(), 12288);
        assert_eq!(nvidia.backend, GpuBackend::Cuda);
        assert_eq!(nvidia.tier, HardwareTier::Mid);
    }

    /// Live check of the DXGI path: it also serves AMD/Intel, where nvidia-smi is powerless.
    #[cfg(windows)]
    #[test]
    fn test_dxgi_sees_adapter_with_vram() {
        let info = detect_dxgi().expect("DXGI должен вернуть хотя бы один адаптер");
        assert!(info.vram_mb.unwrap_or(0) > 512);
        assert!(info.name.is_some());
    }
}

