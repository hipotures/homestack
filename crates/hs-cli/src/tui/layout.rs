//! Which panels fit the window and where (mockups, `00-system.txt` part 5).
//! A panel shows when the user has it on and it fits, in the order
//! workspaces, cluster, storage, log; width and height count separately.

use ratatui::layout::Rect;

pub const MIN_WIDTH: u16 = 80;
pub const MIN_HEIGHT: u16 = 24;
/// Below this width storage and log go under the list, not beside it.
const WIDE: u16 = 120;
const WIDER: u16 = 160;
/// Rows the workspaces panel keeps when storage and log go under it.
const LIST_MIN: u16 = 16;
/// Height of storage and log when they go under the list.
const UNDER: u16 = 8;

/// The panels the user has on (digits 1–4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shown {
    pub workspaces: bool,
    pub cluster: bool,
    pub storage: bool,
    pub log: bool,
}

impl Shown {
    #[must_use]
    pub fn all() -> Self {
        Self {
            workspaces: true,
            cluster: true,
            storage: true,
            log: true,
        }
    }

    fn count(self) -> usize {
        [self.workspaces, self.cluster, self.storage, self.log]
            .into_iter()
            .filter(|on| *on)
            .count()
    }

    /// Show or hide the panel of `digit`; the last panel shown stays.
    pub fn toggle(&mut self, digit: u8) {
        let slot = match digit {
            1 => &mut self.workspaces,
            2 => &mut self.cluster,
            3 => &mut self.storage,
            4 => &mut self.log,
            _ => return,
        };
        let hiding = *slot;
        *slot = !*slot;
        if hiding && self.count() == 0 {
            *self = Self {
                workspaces: digit == 1,
                cluster: digit == 2,
                storage: digit == 3,
                log: digit == 4,
            };
        }
    }
}

/// Where each panel goes; `None` is not drawn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Screen {
    pub too_small: bool,
    pub cluster: Option<Rect>,
    pub workspaces: Option<Rect>,
    pub storage: Option<Rect>,
    pub log: Option<Rect>,
}

/// Rows of the cluster panel for a window `height` high.
fn cluster_height(height: u16) -> u16 {
    if height >= 45 {
        8
    } else if height >= 36 {
        6
    } else {
        3
    }
}

/// Storage on top, log under it, in `area`.
fn stack(area: Rect, storage: bool, log: bool, storage_height: u16) -> (Option<Rect>, Option<Rect>) {
    match (storage, log) {
        (true, true) => {
            let top = storage_height.min(area.height.saturating_sub(3));
            (
                Some(Rect::new(area.x, area.y, area.width, top)),
                Some(Rect::new(area.x, area.y + top, area.width, area.height - top)),
            )
        }
        (true, false) => (Some(area), None),
        (false, true) => (None, Some(area)),
        (false, false) => (None, None),
    }
}

#[must_use]
pub fn arrange(area: Rect, shown: Shown) -> Screen {
    let mut screen = Screen::default();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        screen.too_small = true;
        return screen;
    }
    let mut rest = area;
    if shown.cluster {
        let height = if shown.count() == 1 {
            area.height
        } else {
            cluster_height(area.height)
        };
        screen.cluster = Some(Rect::new(area.x, area.y, area.width, height));
        rest = Rect::new(area.x, area.y + height, area.width, area.height - height);
    }
    if rest.height == 0 {
        return screen;
    }
    let extra = shown.storage || shown.log;
    if !shown.workspaces {
        let half = if shown.storage && shown.log {
            rest.height / 2
        } else {
            rest.height
        };
        (screen.storage, screen.log) = stack(rest, shown.storage, shown.log, half);
        return screen;
    }
    if !extra {
        screen.workspaces = Some(rest);
        return screen;
    }
    if area.width >= WIDE {
        let column = if area.width >= WIDER { 48 } else { 36 };
        let storage_height = if area.height >= 45 { 15 } else { 12 };
        let list = Rect::new(rest.x, rest.y, rest.width - column, rest.height);
        let right = Rect::new(rest.x + list.width, rest.y, column, rest.height);
        screen.workspaces = Some(list);
        (screen.storage, screen.log) = stack(right, shown.storage, shown.log, storage_height);
        return screen;
    }
    // Narrow: storage and log go under the list while it keeps LIST_MIN rows.
    let wanted = UNDER * (u16::from(shown.storage) + u16::from(shown.log));
    let (storage, log, under) = if rest.height >= LIST_MIN + wanted {
        (shown.storage, shown.log, wanted)
    } else if shown.storage && rest.height >= LIST_MIN + UNDER {
        (true, false, UNDER)
    } else if shown.log && rest.height >= LIST_MIN + UNDER {
        (false, true, UNDER)
    } else {
        (false, false, 0)
    };
    let list_height = rest.height - under;
    screen.workspaces = Some(Rect::new(rest.x, rest.y, rest.width, list_height));
    if under > 0 {
        let below = Rect::new(rest.x, rest.y + list_height, rest.width, under);
        (screen.storage, screen.log) = stack(below, storage, log, UNDER);
    }
    screen
}
