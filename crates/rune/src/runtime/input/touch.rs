//! Touchscreen and pen input, as a browser handles it: the first finger is the primary
//! button. It presses where it lands, so a tap clicks; dragging an element that drags
//! (`El::on_drag`: a slider, a scroll bar) drags it; any other drag pans the scroll viewport
//! under the finger (`runtime/scroll/touch.rs`), flinging it on with momentum, and cancels
//! the press. A finger held still for [`LONG_PRESS`] is a secondary click there, as Chromium's
//! long press sends `contextmenu`: the press it began lets go without a click and the menu opens
//! under the finger, which then lifts on nothing. Further fingers are ignored while one is down.
use super::*;
use std::collections::VecDeque;

/// How far (points) a finger travels before its touch is a drag or a pan, not a tap.
const TOUCH_SLOP: f32 = 8.0;
/// A finger held this long (seconds) before lifting throws nothing.
const FLING_HOLD: f64 = 0.1;
/// A fling takes its speed from the finger's last this many seconds, as Chromium's velocity
/// tracker does: one event alone misreads a finger that slows as it lifts.
const FLING_WINDOW: f64 = 0.1;
/// A finger held still this long (seconds) opens the menu under it (Chromium's
/// `long_press_time_in_ms`).
const LONG_PRESS: f64 = 0.5;

/// A host's touch phase, as winit reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TouchPhase {
    Started,
    Moved,
    Ended,
    /// The system took the touch (a gesture of its own): nothing it pressed is clicked.
    Cancelled,
}

/// What touches are doing: the finger that is down, and the viewport a touch holds or flung.
#[derive(Default)]
pub(super) struct Touch {
    pub(super) scroll: Option<scroll_touch::TouchScroll>,
    finger: Option<Finger>,
}

struct Finger {
    id: u64,
    origin: (f32, f32),
    /// (time, travel along the pan's axis) of the events in the last `FLING_WINDOW`.
    samples: VecDeque<(f64, f32)>,
    mode: Mode,
    /// When (clock seconds), still pending, it becomes a long press.
    hold_due: f64,
    /// The vertical viewport under the finger, held until the pan picks its axis.
    scroll: Option<NodeId>,
    horizontal: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Still within the slop: lifting it here is a tap.
    Pending,
    Drag,
    Pan,
    /// A long press opened the menu under it: it lifts on nothing.
    Menu,
}

impl<S: 'static> Runtime<S> {
    /// One touch event at (x, y) in window points, `time` in seconds on any steady clock. A
    /// host with touches calls this for every one; `id` tells fingers apart.
    pub fn touch(&mut self, id: u64, phase: TouchPhase, x: f32, y: f32, time: f64) {
        match phase {
            TouchPhase::Started => self.touch_down(id, x, y, time),
            TouchPhase::Moved => self.touch_move(id, x, y, time),
            TouchPhase::Ended | TouchPhase::Cancelled => self.touch_up(id, x, y, time, phase == TouchPhase::Cancelled),
        }
    }

    /// Whether a finger is down: a host ignores the mouse events its system makes up from it.
    pub fn touching(&self) -> bool {
        self.touch.finger.is_some()
    }

    fn touch_down(&mut self, id: u64, x: f32, y: f32, time: f64) {
        if self.touch.finger.is_some() { return; }
        let scroll = self.touch_scroll_start(x, y);
        self.pointer_move(x, y);
        self.pointer_down(x, y);
        let hold_due = self.clock + LONG_PRESS;
        self.schedule_wakes([std::time::Duration::from_secs_f64(LONG_PRESS)]);
        self.touch.finger = Some(Finger { id, origin: (x, y), samples: VecDeque::from([(time, 0.0)]), mode: Mode::Pending, hold_due, scroll, horizontal: false });
    }

    fn touch_move(&mut self, id: u64, x: f32, y: f32, time: f64) {
        let Some(f) = self.touch.finger.as_ref().filter(|f| f.id == id) else { return };
        let (dx, dy) = (x - f.origin.0, y - f.origin.1);
        if f.mode == Mode::Pending && dx.hypot(dy) > TOUCH_SLOP {
            let (origin, scroll) = (f.origin, f.scroll);
            let (scroll, horizontal) = if self.press_drags() { (None, false) } else { self.touch_scroll_pan(origin.0, origin.1, dx.abs() > dy.abs(), scroll) };
            // A pan isn't a selection: the press it began lets go of the text it landed on, or
            // its release would select all that scrolled past.
            if scroll.is_some() { self.text_sel.dragging = false; }
            let f = self.touch.finger.as_mut().unwrap();
            (f.scroll, f.horizontal, f.mode) = (scroll, horizontal, if scroll.is_some() { Mode::Pan } else { Mode::Drag });
            // The samples so far ran along y.
            if horizontal { f.samples = VecDeque::from([(time, dx)]); }
        }
        let f = self.touch.finger.as_mut().unwrap();
        let along = if f.horizontal { dx } else { dy };
        f.samples.push_back((time, along));
        while f.samples.front().is_some_and(|&(t, _)| t < time - FLING_WINDOW) { f.samples.pop_front(); }
        match (f.mode, f.scroll) {
            (Mode::Pan, Some(scroll)) => self.touch_scroll_move(scroll, along),
            (Mode::Drag, _) => self.pointer_move(x, y),
            _ => {}
        }
    }

    fn touch_up(&mut self, id: u64, x: f32, y: f32, time: f64, cancelled: bool) {
        let Some(f) = self.touch.finger.take_if(|f| f.id == id) else { return };
        let pan = f.mode == Mode::Pan;
        if f.mode == Mode::Menu {
            self.pointer_leave();
            return;
        }
        let held = f.samples.back().is_none_or(|&(at, _)| time - at >= FLING_HOLD);
        let velocity = if pan && !cancelled && !held { slope(&f.samples) } else { 0.0 };
        self.touch_scroll_end(f.scroll, velocity);
        if cancelled {
            self.cancel_pointer();
            return;
        }
        // A pan cancelled the press, so its release clicks nothing.
        let (x, y) = if pan { f.origin } else { (x, y) };
        if f.mode == Mode::Drag { self.pointer_move(x, y); }
        self.pointer_up(x, y);
        // No hover is left under a lifted finger.
        self.pointer_leave();
    }

    /// A finger still within the slop when its long press is due: the press it began lets go
    /// without a click (a drag it started hears a cancelled `Phase::Up`), and a secondary click
    /// lands where it went down.
    pub(in crate::runtime) fn fire_touch_hold(&mut self) {
        let Some(f) = self.touch.finger.as_ref().filter(|f| f.mode == Mode::Pending && self.clock + 1e-6 >= f.hold_due) else { return };
        let ((x, y), scroll) = (f.origin, f.scroll);
        self.touch_scroll_end(scroll, 0.0);
        if !self.cancel_drag() {
            self.press.drop_held();
            self.text_sel.dragging = false;
            self.end_press_repeat();
        }
        self.secondary_down(x, y);
        self.secondary_up(x, y);
        if let Some(f) = self.touch.finger.as_mut() { (f.mode, f.scroll) = (Mode::Menu, None); }
    }

    /// Whether the held press is on an element that drags, so a finger drags it, not the page.
    fn press_drags(&self) -> bool {
        let Some(path) = self.press.path() else { return false };
        self.live_index(path).is_some_and(|i| self.nodes[i].handlers.on_drag.is_some() && !self.drags_out(&self.nodes[i]))
    }
}

/// Points per second: the least-squares slope of travel over time.
fn slope(samples: &VecDeque<(f64, f32)>) -> f32 {
    let n = samples.len() as f64;
    let (mt, ma) = samples.iter().fold((0.0, 0.0), |(t, a), &(st, sa)| (t + st / n, a + f64::from(sa) / n));
    let (cov, var) = samples.iter().fold((0.0, 0.0), |(c, v), &(st, sa)| (c + (st - mt) * (f64::from(sa) - ma), v + (st - mt).powi(2)));
    if var > 0.0 { (cov / var) as f32 } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PointerPress, ScrollSpec, col, el, text};

    #[derive(Default)]
    struct S { taps: Vec<usize>, drags: Vec<f32>, menus: Vec<(usize, f32, f32)> }

    fn view(_: &S, _: &mut Cx<S>) -> El {
        col().size(200.0, 300.0)
            .child(el().key("knob").size(200.0, 40.0).on_drag(|s: &mut S, d: &mut Drag, _| s.drags.push(d.dx)))
            .child(col().key("list").w_full().h(200.0).scroll(ScrollSpec { stick_bottom: false })
                .children((0..30).map(|i| el().key(format!("row{i}")).w_full().h(40.0).on_press(move |s: &mut S| s.taps.push(i))
                    .on_secondary_press(move |s: &mut S, p: PointerPress, _| s.menus.push((i, p.x, p.y))))))
    }

    fn rig() -> Runtime<S> {
        let mut rt = Runtime::new(S::default(), view, (200.0, 300.0), Env::default());
        rt.frame(0.0);
        rt
    }

    fn offset(rt: &Runtime<S>) -> f64 {
        let id = rt.nodes.iter().find(|n| n.key.as_deref() == Some("list")).unwrap().id;
        rt.scrolls[&id].offset
    }

    /// A finger from (x, y) moving by `dy` over `steps` events 10 ms apart, then lifted.
    fn swipe(rt: &mut Runtime<S>, (x, y): (f32, f32), dy: f32, steps: usize) {
        rt.touch(1, TouchPhase::Started, x, y, 0.0);
        for i in 1..=steps {
            rt.touch(1, TouchPhase::Moved, x, y + dy * i as f32 / steps as f32, i as f64 * 0.01);
        }
        rt.touch(1, TouchPhase::Ended, x, y + dy, steps as f64 * 0.01 + 0.005);
        rt.frame(1.0 / 60.0);
    }

    #[test]
    fn a_tap_clicks_what_it_lands_on_and_leaves_no_hover() {
        let mut rt = rig();
        rt.touch(1, TouchPhase::Started, 100.0, 70.0, 0.0);
        rt.touch(1, TouchPhase::Moved, 103.0, 72.0, 0.02);
        rt.touch(1, TouchPhase::Ended, 103.0, 72.0, 0.05);
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.state.taps, [0]);
        assert_eq!(rt.pointer(), None);
        assert!(!rt.touching());
    }

    #[test]
    fn a_drag_on_a_list_scrolls_it_without_clicking_and_flings_on() {
        let mut rt = rig();
        swipe(&mut rt, (100.0, 200.0), -100.0, 5);
        assert!(rt.state.taps.is_empty(), "a pan presses nothing");
        let lifted = offset(&rt);
        assert!(lifted >= 90.0, "the list followed the finger: {lifted}");
        for _ in 0..30 { rt.frame(1.0 / 60.0); }
        assert!(offset(&rt) > lifted + 50.0, "and coasted on: {} after {lifted}", offset(&rt));
    }

    #[test]
    fn a_finger_held_still_before_lifting_throws_nothing() {
        let mut rt = rig();
        rt.touch(1, TouchPhase::Started, 100.0, 200.0, 0.0);
        rt.touch(1, TouchPhase::Moved, 100.0, 150.0, 0.01);
        rt.touch(1, TouchPhase::Ended, 100.0, 150.0, 0.5);
        for _ in 0..30 { rt.frame(1.0 / 60.0); }
        assert_eq!(offset(&rt), 50.0);
    }

    #[test]
    fn a_drag_on_an_element_that_drags_drags_it_not_the_page() {
        let mut rt = rig();
        rt.touch(1, TouchPhase::Started, 20.0, 20.0, 0.0);
        rt.touch(1, TouchPhase::Moved, 60.0, 30.0, 0.01);
        rt.touch(1, TouchPhase::Ended, 60.0, 30.0, 0.02);
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.state.drags.last(), Some(&40.0));
        assert_eq!(offset(&rt), 0.0);
    }

    fn prose(_: &S, _: &mut Cx<S>) -> El {
        col().key("list").size(200.0, 300.0).scroll(ScrollSpec { stick_bottom: false }).selection_group(true)
            .children((0..30).map(|i| text(format!("Line {i} of selectable prose")).key(format!("t{i}")).w_full().h(40.0).selectable(true)))
    }

    #[test]
    fn a_pan_over_selectable_text_selects_nothing() {
        let mut rt = Runtime::new(S::default(), prose, (200.0, 300.0), Env::default());
        rt.frame(0.0);
        // Drawn between events, as a screen is, so other text passes under the finger.
        rt.touch(1, TouchPhase::Started, 100.0, 250.0, 0.0);
        for i in 1..=10 {
            rt.touch(1, TouchPhase::Moved, 100.0, 250.0 - 20.0 * i as f32, i as f64 * 0.01);
            rt.frame(0.01);
        }
        rt.touch(1, TouchPhase::Ended, 100.0, 50.0, 0.105);
        for _ in 0..30 { rt.frame(1.0 / 60.0); }
        assert!(offset(&rt) > 150.0, "the text scrolled: {}", offset(&rt));
        assert_eq!(rt.selected_text(), None);
    }

    #[test]
    fn a_fling_keeps_its_speed_when_the_finger_slows_as_it_lifts() {
        let mut rt = rig();
        // 2000 points a second for 50 ms, then the last event only 2 points on, as a finger
        // that slows as it lifts reports.
        rt.touch(1, TouchPhase::Started, 100.0, 230.0, 0.0);
        for i in 1..=5 { rt.touch(1, TouchPhase::Moved, 100.0, 230.0 - 20.0 * i as f32, i as f64 * 0.01); }
        rt.touch(1, TouchPhase::Moved, 100.0, 128.0, 0.06);
        rt.touch(1, TouchPhase::Ended, 100.0, 128.0, 0.065);
        let lifted = offset(&rt);
        for _ in 0..120 { rt.frame(1.0 / 60.0); }
        assert!(offset(&rt) > lifted + 400.0, "coasted {} points", offset(&rt) - lifted);
    }

    #[test]
    fn a_finger_held_still_opens_the_menu_there_and_lifts_on_nothing() {
        let mut rt = rig();
        rt.touch(1, TouchPhase::Started, 100.0, 70.0, 0.0);
        rt.frame(0.3);
        rt.touch(1, TouchPhase::Moved, 103.0, 72.0, 0.3);
        assert!(rt.state.menus.is_empty(), "not yet");
        rt.frame(0.25);
        assert_eq!(rt.state.menus, [(0, 100.0, 70.0)]);
        rt.touch(1, TouchPhase::Moved, 100.0, 20.0, 0.6);
        rt.touch(1, TouchPhase::Ended, 100.0, 20.0, 0.7);
        rt.frame(1.0 / 60.0);
        assert!(rt.state.taps.is_empty(), "the lift clicks nothing");
        assert_eq!(offset(&rt), 0.0, "and pans nothing");
    }

    #[test]
    fn a_tap_or_a_pan_opens_no_menu() {
        let mut rt = rig();
        rt.touch(1, TouchPhase::Started, 100.0, 70.0, 0.0);
        rt.touch(1, TouchPhase::Ended, 100.0, 70.0, 0.1);
        rt.frame(0.6);
        swipe(&mut rt, (100.0, 200.0), -100.0, 5);
        rt.frame(0.6);
        assert_eq!(rt.state.taps, [0]);
        assert!(rt.state.menus.is_empty());
    }

    #[test]
    fn a_second_finger_is_ignored_and_a_cancelled_touch_clicks_nothing() {
        let mut rt = rig();
        rt.touch(1, TouchPhase::Started, 100.0, 70.0, 0.0);
        rt.touch(2, TouchPhase::Started, 100.0, 110.0, 0.0);
        rt.touch(2, TouchPhase::Ended, 100.0, 110.0, 0.01);
        rt.touch(1, TouchPhase::Cancelled, 100.0, 70.0, 0.02);
        rt.frame(1.0 / 60.0);
        assert!(rt.state.taps.is_empty());
        assert!(!rt.touching());
    }
}
