//! Which panels fit the window and where (mockups, `00-system.txt` part 5).
//! A panel shows when the user has it on and it fits, in the order
//! workspaces, cluster, storage, log; width and height count separately.

use ratatui::layout::Rect;

pub const MIN_WIDTH: u16 = 80;
pub const MIN_HEIGHT: u16 = 24;
/// From this width the storage column under the list is wider.
const WIDER: u16 = 160;
/// Rows the workspaces panel keeps when storage and log go under it.
const LIST_MIN: u16 = 16;
/// Height of storage and log under the list (three nodes' bars fit), and
/// in a window 45 rows high and more.
const UNDER: u16 = 8;
const UNDER_TALL: u16 = 10;

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

/// Storage and log side by side in `area`: the log on the left, storage in
/// a column on the right; one of them alone takes the whole width.
fn beside(area: Rect, storage: bool, log: bool) -> (Option<Rect>, Option<Rect>) {
    match (storage, log) {
        (true, true) => {
            let column = if area.width >= WIDER { 48 } else { 36 };
            let left = area.width - column;
            (
                Some(Rect::new(area.x + left, area.y, column, area.height)),
                Some(Rect::new(area.x, area.y, left, area.height)),
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
    if !shown.workspaces {
        (screen.storage, screen.log) = beside(rest, shown.storage, shown.log);
        return screen;
    }
    // Storage and log go under the list, the whole width, while it keeps
    // LIST_MIN rows.
    let under = if area.height >= 45 { UNDER_TALL } else { UNDER };
    if !(shown.storage || shown.log) || rest.height < LIST_MIN + under {
        screen.workspaces = Some(rest);
        return screen;
    }
    let list_height = rest.height - under;
    screen.workspaces = Some(Rect::new(rest.x, rest.y, rest.width, list_height));
    let below = Rect::new(rest.x, rest.y + list_height, rest.width, under);
    (screen.storage, screen.log) = beside(below, shown.storage, shown.log);
    screen
}
