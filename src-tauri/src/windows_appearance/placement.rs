use tauri::{PhysicalPosition, PhysicalSize, Runtime, Window};

#[derive(Clone, Copy, Debug, PartialEq)]
struct Bounds {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

fn clamp_bounds(rect: Bounds, work: Bounds) -> Bounds {
    let width = rect.width.min(work.width);
    let height = rect.height.min(work.height);
    Bounds {
        x: (rect.x as i64).clamp(work.x as i64, work.x as i64 + (work.width - width) as i64) as i32,
        y: (rect.y as i64).clamp(work.y as i64, work.y as i64 + (work.height - height) as i64)
            as i32,
        width,
        height,
    }
}

pub(super) fn keep_visible<R: Runtime>(window: &Window<R>) -> tauri::Result<()> {
    let maximized = window.is_maximized()?;
    if maximized {
        window.unmaximize()?;
    }
    let result = keep_normal_bounds_visible(window);
    if maximized {
        window.maximize()?;
    }
    result
}

fn keep_normal_bounds_visible<R: Runtime>(window: &Window<R>) -> tauri::Result<()> {
    let position = window.outer_position()?;
    let outer = window.outer_size()?;
    let inner = window.inner_size()?;
    let monitors = window.available_monitors()?;
    let nearest = monitors.iter().min_by_key(|monitor| {
        let work = monitor.work_area();
        let x = position.x as i64 + outer.width as i64 / 2;
        let y = position.y as i64 + outer.height as i64 / 2;
        let dx = x - x.clamp(
            work.position.x as i64,
            work.position.x as i64 + work.size.width as i64,
        );
        let dy = y - y.clamp(
            work.position.y as i64,
            work.position.y as i64 + work.size.height as i64,
        );
        dx as i128 * dx as i128 + dy as i128 * dy as i128
    });
    if let Some(monitor) = nearest {
        let area = monitor.work_area();
        let original = Bounds {
            x: position.x,
            y: position.y,
            width: outer.width,
            height: outer.height,
        };
        let adjusted = clamp_bounds(
            original,
            Bounds {
                x: area.position.x,
                y: area.position.y,
                width: area.size.width,
                height: area.size.height,
            },
        );
        if adjusted.width != outer.width || adjusted.height != outer.height {
            window.set_size(PhysicalSize::new(
                adjusted
                    .width
                    .saturating_sub(outer.width.saturating_sub(inner.width))
                    .max(1),
                adjusted
                    .height
                    .saturating_sub(outer.height.saturating_sub(inner.height))
                    .max(1),
            ))?;
        }
        if adjusted.x != position.x || adjusted.y != position.y {
            window.set_position(PhysicalPosition::new(adjusted.x, adjusted.y))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_bounds_are_unchanged() {
        let window = Bounds {
            x: 80,
            y: 50,
            width: 1024,
            height: 768,
        };
        assert_eq!(
            clamp_bounds(
                window,
                Bounds {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1040
                }
            ),
            window
        );
    }

    #[test]
    fn removed_monitor_and_smaller_work_area_recover_titlebar() {
        assert_eq!(
            clamp_bounds(
                Bounds {
                    x: 2800,
                    y: -900,
                    width: 2000,
                    height: 1600
                },
                Bounds {
                    x: 0,
                    y: 40,
                    width: 1280,
                    height: 680
                }
            ),
            Bounds {
                x: 0,
                y: 40,
                width: 1280,
                height: 680
            }
        );
    }

    #[test]
    fn negative_monitor_coordinates_are_preserved() {
        assert_eq!(
            clamp_bounds(
                Bounds {
                    x: -1400,
                    y: 10,
                    width: 1000,
                    height: 700
                },
                Bounds {
                    x: -1920,
                    y: 0,
                    width: 1920,
                    height: 1040
                }
            ),
            Bounds {
                x: -1400,
                y: 10,
                width: 1000,
                height: 700
            }
        );
    }
}
