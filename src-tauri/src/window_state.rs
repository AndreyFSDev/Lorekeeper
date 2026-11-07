//! Main window geometry in the program folder.
//! The stock tauri-plugin-window-state writes the state to `%APPDATA%\<identifier>`,
//! which contradicts the rule "everything lives in one installation folder".

use crate::paths;

/// Minimum window size — the same as `minWidth`/`minHeight` in tauri.conf.json.
const MIN_WIDTH: u32 = 900;
const MIN_HEIGHT: u32 = 620;
/// Default window size (from tauri.conf.json) — for the case when there is no saved
/// geometry yet and the window is already maximized.
const DEFAULT_WIDTH: u32 = 1100;
const DEFAULT_HEIGHT: u32 = 740;

/// Window geometry on disk (`data/window.json`).
///
/// `width`/`height` — the inner size (it is what `set_size` drives),
/// `outer_width`/`outer_height` — the whole window block with frame and title:
/// binding to the work area is computed from the outer pair (A1-02), otherwise the frame
/// and title stuck out past the screen edge. Every field has `#[serde(default)]` —
/// files written before a field appeared must keep loading.
#[derive(serde::Serialize, serde::Deserialize)]
struct WindowGeometry {
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
    /// Outer position — the top-left corner of the frame.
    #[serde(default)]
    x: i32,
    #[serde(default)]
    y: i32,
    /// The window was maximized to the full screen.
    #[serde(default)]
    maximized: bool,
    /// Outer size. 0 in files written before this field: the frame is unknown,
    /// the binding counts the outer size as equal to the inner one.
    #[serde(default)]
    outer_width: u32,
    #[serde(default)]
    outer_height: u32,
}

impl Default for WindowGeometry {
    fn default() -> Self {
        Self {
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            x: 0,
            y: 0,
            maximized: false,
            outer_width: DEFAULT_WIDTH,
            outer_height: DEFAULT_HEIGHT,
        }
    }
}

impl WindowGeometry {
    /// Outer size (the whole window, with decorations). Old files store only the
    /// inner one — for them the frame is unknown, count it as zero.
    fn outer_size(&self) -> (u32, u32) {
        (
            if self.outer_width == 0 { self.width } else { self.outer_width },
            if self.outer_height == 0 { self.height } else { self.outer_height },
        )
    }
}

/// Monitor work area (without the taskbar), physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

fn state_path() -> std::path::PathBuf {
    paths::data_dir().join("window.json")
}

/// Read the saved geometry. No file — `None`; the file exists but did not
/// read or parse — that is a V-09 error (a `.corrupt` copy has already been
/// made by `json_store`), not "no state": the next write has no
/// right to overwrite such a file with a default.
fn load_from(path: &std::path::Path) -> Option<WindowGeometry> {
    crate::json_store::read_state(path).unwrap_or(None)
}

fn load() -> Option<WindowGeometry> {
    load_from(&state_path())
}

/// Save the geometry atomically (tmp + fsync + rename, the shared `json_store`).
fn write_to(geometry: &WindowGeometry, path: &std::path::Path) -> Result<(), String> {
    crate::json_store::save_value(geometry, path)
}

fn write(geometry: &WindowGeometry) -> Result<(), String> {
    write_to(geometry, &state_path())
}

/// The `save` branch for a maximized/minimized window: the previous geometry
/// is kept, only the maximized flag changes. Refusal to write over
/// a file that exists but does not read/parse (V-09): unknown
/// content is an error, not default geometry.
fn keep_geometry_and_set_flag(path: &std::path::Path, maximized: bool) -> Result<(), String> {
    let mut geometry = crate::json_store::read_state::<WindowGeometry>(path)?.unwrap_or_default();
    geometry.maximized = maximized;
    write_to(&geometry, path)
}

/// Restore the window's size, position and maximized state.
/// The geometry is always clamped to the monitor's work area: a window saved
/// off-screen (a second monitor was disconnected, the resolution changed) comes back
/// fully visible instead of "getting lost" past the edge.
/// Runs on the main thread (Tauri window events); there is no disk IO here.
/// Failures of `set_size`/`set_position`/`maximize` are logged, not swallowed (A1-01):
/// a window that refused to move is a visible degradation with a trace in boot.log.
pub fn restore(window: &tauri::WebviewWindow) {
    let Some(geometry) = load() else {
        return;
    };
    let monitors: Vec<Rect> = window
        .available_monitors()
        .unwrap_or_default()
        .iter()
        .map(|m| rect_of(m.work_area()))
        .collect();
    let primary = window.primary_monitor().ok().flatten().map(|m| rect_of(m.work_area()));

    let work = match monitor_for(&monitors, &geometry).or(primary) {
        Some(work) => work,
        // No monitors visible — leave the window as is, do not touch the position.
        None => return,
    };
    let fitted = fit_to_work_area(&geometry, work);

    // Position first: moving to a monitor with a different scale recomputes the
    // window size by DPI, and a physical size set before the move got
    // squeezed (900×620 at 125% → ~720×500 at 100%).
    // set_size drives the INNER size, the position is outer (the frame corner).
    if let Err(e) = window.set_position(tauri::PhysicalPosition::new(fitted.x, fitted.y)) {
        crate::boot_log(&format!("Окно: положение не восстановлено ({e})"));
    }
    if let Err(e) = window.set_size(tauri::PhysicalSize::new(fitted.width, fitted.height)) {
        crate::boot_log(&format!("Окно: размер не восстановлен ({e})"));
    }
    if geometry.maximized {
        if let Err(e) = window.maximize() {
            crate::boot_log(&format!("Окно: развёрнутость не восстановлена ({e})"));
        }
    }
}

fn rect_of(area: &tauri::PhysicalRect<i32, u32>) -> Rect {
    Rect {
        x: area.position.x,
        y: area.position.y,
        width: area.size.width,
        height: area.size.height,
    }
}

/// The monitor the window was on: the largest intersection with the saved
/// rectangle. Nothing intersects (the monitor is disconnected) — None.
fn monitor_for(monitors: &[Rect], geometry: &WindowGeometry) -> Option<Rect> {
    let mut best: Option<(u64, Rect)> = None;
    for monitor in monitors {
        let overlap = overlap_area(*monitor, geometry);
        if overlap > 0 && best.map(|(area, _)| overlap > area).unwrap_or(true) {
            best = Some((overlap, *monitor));
        }
    }
    best.map(|(_, monitor)| monitor)
}

fn overlap_area(monitor: Rect, geometry: &WindowGeometry) -> u64 {
    // The window in space — its OUTER rectangle: the frame is part of it.
    let (outer_width, outer_height) = geometry.outer_size();
    let left = monitor.x.max(geometry.x);
    let top = monitor.y.max(geometry.y);
    // Edges are counted in i64: a broken window.json (x = i32::MAX, width = 3000)
    // overflowed i32 in debug and gave garbage in release.
    let monitor_right = i64::from(monitor.x) + i64::from(monitor.width);
    let geometry_right = i64::from(geometry.x) + i64::from(outer_width);
    let monitor_bottom = i64::from(monitor.y) + i64::from(monitor.height);
    let geometry_bottom = i64::from(geometry.y) + i64::from(outer_height);
    let right = monitor_right.min(geometry_right);
    let bottom = monitor_bottom.min(geometry_bottom);
    let left = i64::from(left);
    let top = i64::from(top);
    if right <= left || bottom <= top {
        return 0;
    }
    ((right - left) as u64) * ((bottom - top) as u64)
}

/// Clamp the geometry to the work area: the OUTER size (with frame and
/// title) within the monitor and not less than the minimum, the window entirely
/// inside the area (title and buttons reachable). Clamping by the inner
/// size (A1-02) let the frame and title out past the work area.
fn fit_to_work_area(geometry: &WindowGeometry, work: Rect) -> WindowGeometry {
    let (outer_width, outer_height) = geometry.outer_size();
    // The frame is the difference between outer and inner; set_size drives the inner size.
    let frame_w = outer_width.saturating_sub(geometry.width);
    let frame_h = outer_height.saturating_sub(geometry.height);
    let width = geometry
        .width
        .clamp(MIN_WIDTH, work.width.saturating_sub(frame_w).max(MIN_WIDTH));
    let height = geometry
        .height
        .clamp(MIN_HEIGHT, work.height.saturating_sub(frame_h).max(MIN_HEIGHT));
    let outer_width = width + frame_w;
    let outer_height = height + frame_h;
    let max_x = (work.x + work.width as i32 - outer_width as i32).max(work.x);
    let max_y = (work.y + work.height as i32 - outer_height as i32).max(work.y);
    WindowGeometry {
        width,
        height,
        x: geometry.x.clamp(work.x, max_x),
        y: geometry.y.clamp(work.y, max_y),
        maximized: geometry.maximized,
        outer_width,
        outer_height,
    }
}

/// Save the window geometry. Runs on the main thread (Tauri window events):
/// a blocking write of one small file, a failure goes to boot.log.
pub fn save(window: &tauri::WebviewWindow) {
    // For a maximized window the size and position are those of the whole screen, for a minimized one
    // the coordinates go to −32000. In both cases save only the maximized
    // flag, and take the geometry from the previous run: otherwise the window would
    // open after a restart the size of the screen or beyond its edges.
    let maximized = window.is_maximized().unwrap_or(false);
    if maximized || window.is_minimized().unwrap_or(false) {
        if let Err(e) = keep_geometry_and_set_flag(&state_path(), maximized) {
            crate::boot_log(&format!("Окно: геометрия не сохранена ({e})"));
        }
        return;
    }

    let size = match window.inner_size() {
        Ok(size) => size,
        Err(e) => {
            crate::boot_log(&format!("Окно: внутренний размер не считан ({e})"));
            return;
        }
    };
    let outer = match window.outer_size() {
        Ok(outer) => outer,
        Err(e) => {
            crate::boot_log(&format!("Окно: внешний размер не считан ({e})"));
            return;
        }
    };
    let position = match window.outer_position() {
        Ok(position) => position,
        Err(e) => {
            crate::boot_log(&format!("Окно: положение не считано ({e})"));
            return;
        }
    };
    // The position is outer (the frame corner) — and the size is stored as a pair: the inner one is
    // driven by `set_size`, the outer one is needed by the work-area binding (A1-02).
    if let Err(e) = write(&WindowGeometry {
        width: size.width,
        height: size.height,
        x: position.x,
        y: position.y,
        maximized: false,
        outer_width: outer.width,
        outer_height: outer.height,
    }) {
        crate::boot_log(&format!("Окно: геометрия не сохранена ({e})"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The "old" file's geometry: the outer size is not saved (0) — the frame
    /// is counted as zero, and the checks below compare against the previous numbers.
    fn geometry(x: i32, y: i32, width: u32, height: u32) -> WindowGeometry {
        WindowGeometry {
            x,
            y,
            width,
            height,
            maximized: false,
            outer_width: 0,
            outer_height: 0,
        }
    }

    const WORK: Rect = Rect {
        x: 0,
        y: 0,
        width: 1920,
        height: 1040,
    };

    #[test]
    fn test_keeps_geometry_inside_work_area() {
        let fitted = fit_to_work_area(&geometry(100, 80, 1100, 740), WORK);
        assert_eq!((fitted.x, fitted.y, fitted.width, fitted.height), (100, 80, 1100, 740));
    }

    #[test]
    fn test_offscreen_position_is_pulled_back() {
        // The window was saved on a disconnected monitor on the right.
        let fitted = fit_to_work_area(&geometry(2400, 300, 1100, 740), WORK);
        assert_eq!(fitted.x, 1920 - 1100);
        assert_eq!(fitted.y, 300);
        // The window is above and to the left of the screen: the title is reachable again.
        let fitted = fit_to_work_area(&geometry(-500, -400, 1100, 740), WORK);
        assert_eq!((fitted.x, fitted.y), (0, 0));
    }

    #[test]
    fn test_size_is_clamped_to_monitor_and_minimum() {
        // A size larger than the monitor was saved — squeeze it.
        let fitted = fit_to_work_area(&geometry(0, 0, 3840, 2160), WORK);
        assert_eq!((fitted.width, fitted.height), (1920, 1040));
        // Junk zeros from a broken file — substitute the minimum.
        let fitted = fit_to_work_area(&geometry(10, 10, 0, 0), WORK);
        assert_eq!((fitted.width, fitted.height), (MIN_WIDTH, MIN_HEIGHT));
    }

    #[test]
    fn test_monitor_pick_by_overlap() {
        let left = Rect {
            x: -1920,
            y: 0,
            width: 1920,
            height: 1040,
        };
        let monitors = [WORK, left];
        // The window is entirely on the left monitor.
        assert_eq!(monitor_for(&monitors, &geometry(-1900, 100, 1100, 740)), Some(left));
        // The window is on the primary one.
        assert_eq!(monitor_for(&monitors, &geometry(200, 100, 1100, 740)), Some(WORK));
        // The window is outside both (the monitor was disconnected) — no monitor found.
        assert_eq!(monitor_for(&monitors, &geometry(5000, 5000, 1100, 740)), None);
    }

    /// Broken geometry must not overflow i32 when computing the intersection.
    #[test]
    fn test_overlap_survives_corrupt_geometry_overflow() {
        let g = geometry(i32::MAX - 10, 0, 3000, 1000);
        assert_eq!(overlap_area(WORK, &g), 0, "окно за краем — пересечения нет");
        let g = geometry(i32::MIN + 10, 0, 3000, 1000);
        assert_eq!(overlap_area(WORK, &g), 0);
        // A normal window inside the work area is counted as before.
        assert_eq!(overlap_area(WORK, &geometry(100, 80, 1100, 740)), 1100 * 740);
    }

    #[test]
    fn test_second_monitor_geometry_survives() {
        // A second monitor on the left: the window on it must not drift to the primary one.
        let left = Rect {
            x: -1920,
            y: 0,
            width: 1920,
            height: 1040,
        };
        let fitted = fit_to_work_area(&geometry(-1500, 200, 1100, 740), left);
        assert_eq!((fitted.x, fitted.y), (-1500, 200));
    }

    /// A1-02: the frame and title are part of the window — binding by the outer size
    /// keeps them inside the work area (by the inner one they stuck out).
    #[test]
    fn test_frame_keeps_decorations_inside_work_area() {
        let framed = WindowGeometry {
            x: 810,
            y: 300,
            width: 1100,
            height: 740,
            maximized: false,
            outer_width: 1116,
            outer_height: 779,
        };
        // The inner rectangle is entirely in the area (the previous clamp would have
        // left it as is), but the frame on the right and bottom went past the edge.
        let fitted = fit_to_work_area(&framed, WORK);
        assert_eq!((fitted.width, fitted.height), (1100, 740), "размер менять не нужно");
        assert_eq!(fitted.x, 1920 - 1116, "рамка справа вылезала за рабочую область");
        assert_eq!(fitted.y, 1040 - 779, "рамка снизу вылезала за рабочую область");
        let (outer_width, outer_height) = fitted.outer_size();
        assert!(fitted.x + outer_width as i32 <= WORK.x + WORK.width as i32);
        assert!(fitted.y + outer_height as i32 <= WORK.y + WORK.height as i32);
    }

    fn temp_state_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("cloufam_test_window_state");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("json.corrupt"));
        path
    }

    /// A1-01/V-09: a broken window.json — a `.corrupt` copy and refusal to write over it.
    #[test]
    fn test_corrupt_state_file_is_backed_up_and_never_overwritten() {
        let path = temp_state_path("corrupt.json");
        std::fs::write(&path, "oops {{{").unwrap();
        assert!(load_from(&path).is_none(), "битый файл — состояния нет");
        let backup = path.with_extension("json.corrupt");
        assert!(backup.exists(), "копия повреждённого файла не создана");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "oops {{{");

        let err = keep_geometry_and_set_flag(&path, true).unwrap_err();
        assert!(err.contains("повреждён"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "oops {{{", "файл перезаписан");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&backup);
    }

    /// Saving a maximized window without a previous file creates it from the default.
    #[test]
    fn test_flag_save_creates_missing_state_file() {
        let path = temp_state_path("missing.json");
        keep_geometry_and_set_flag(&path, true).unwrap();
        let geometry = load_from(&path).expect("файл должен записаться");
        assert!(geometry.maximized);
        assert_eq!((geometry.width, geometry.height), (DEFAULT_WIDTH, DEFAULT_HEIGHT));
        let _ = std::fs::remove_file(&path);
    }

    /// A partial file (fields without values) must load, not be rejected:
    /// all fields have `#[serde(default)]`.
    #[test]
    fn test_partial_geometry_deserializes_with_defaults() {
        let geometry: WindowGeometry =
            serde_json::from_str(r#"{"width": 1000}"#).expect("частичный файл обязан читаться");
        assert_eq!(geometry.width, 1000);
        assert!(!geometry.maximized);
        // The outer size is not saved — the frame is counted as zero.
        assert_eq!(geometry.outer_size(), (1000, 0));
    }
}

