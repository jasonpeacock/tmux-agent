//! Pane state transitions, window rollup and the summary. Pure: no tmux, no I/O.
//!
//! The transition table is in docs/design.md, "Transitions".

use std::fmt;

/// Where the agent's main loop stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Loop {
    Idle,
    Working,
    Done,
    Error,
}

/// What a pane shows, ordered by rank: the window shows its highest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Display {
    Idle,
    Working,
    Done,
    NeedsInput,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaneState {
    pub lp: Loop,
    /// At least one prompt is open.
    pub asks: bool,
    /// Background work still running after the main turn ended.
    pub bg: u32,
}

/// A report that has passed the ownership check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The pane was unowned or its owner dead.
    Claim,
    /// A reload or worker respawn of the current owner.
    Reclaim,
    Reset,
    Working,
    Ask,
    Resume,
    Done {
        bg: u32,
    },
    Error,
    Cancel,
    Focus,
    Release,
}

impl PaneState {
    pub const IDLE: PaneState = PaneState {
        lp: Loop::Idle,
        asks: false,
        bg: 0,
    };

    pub fn display(&self) -> Display {
        if self.asks || self.lp == Loop::Error {
            return Display::NeedsInput;
        }
        match self.lp {
            Loop::Idle => Display::Idle,
            Loop::Working => Display::Working,
            Loop::Done => Display::Done,
            Loop::Error => unreachable!(),
        }
    }
}

/// Applies `ev` to a pane. `None` is a pane with no agent. `focused` is true
/// when some client has the pane focused right now.
pub fn apply(prev: Option<PaneState>, ev: Event, focused: bool) -> Option<PaneState> {
    use Event::*;
    let cur = match (prev, ev) {
        (_, Release) => return None,
        (_, Claim) | (_, Reset) => return Some(PaneState::IDLE),
        // Any other report on an unowned pane is an implicit claim first.
        (None, _) => PaneState::IDLE,
        (Some(s), _) => s,
    };
    let next = match ev {
        Reclaim => cur,
        Working => PaneState {
            lp: Loop::Working,
            bg: 0,
            ..cur
        },
        Ask => PaneState { asks: true, ..cur },
        Resume => {
            let lp = match cur.lp {
                Loop::Done | Loop::Idle => Loop::Working,
                other => other,
            };
            PaneState {
                lp,
                asks: false,
                ..cur
            }
        }
        Done { bg } if bg > 0 => PaneState {
            lp: Loop::Working,
            bg,
            ..cur
        },
        Done { .. } => {
            let lp = if focused { Loop::Idle } else { Loop::Done };
            PaneState { lp, bg: 0, ..cur }
        }
        Error => PaneState {
            lp: Loop::Error,
            ..cur
        },
        Cancel => PaneState {
            lp: Loop::Idle,
            asks: false,
            ..cur
        },
        Focus => {
            let lp = match cur.lp {
                Loop::Done | Loop::Error => Loop::Idle,
                other => other,
            };
            PaneState { lp, ..cur }
        }
        Claim | Reset | Release => unreachable!(),
    };
    Some(next)
}

/// The window's state: its highest-ranked pane, or `None` when nothing shows.
pub fn rollup<I: IntoIterator<Item = Display>>(panes: I) -> Option<Display> {
    panes.into_iter().max().filter(|d| *d != Display::Idle)
}

/// Agent pane counts by displayed state, idle excluded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub needs_input: u32,
    pub done: u32,
    pub working: u32,
}

impl Summary {
    pub fn add(&mut self, d: Display) {
        match d {
            Display::NeedsInput => self.needs_input += 1,
            Display::Done => self.done += 1,
            Display::Working => self.working += 1,
            Display::Idle => {}
        }
    }
}

impl Loop {
    pub fn as_str(self) -> &'static str {
        match self {
            Loop::Idle => "idle",
            Loop::Working => "working",
            Loop::Done => "done",
            Loop::Error => "error",
        }
    }

    pub fn parse(s: &str) -> Option<Loop> {
        Some(match s {
            "idle" => Loop::Idle,
            "working" => Loop::Working,
            "done" => Loop::Done,
            "error" => Loop::Error,
            _ => return None,
        })
    }
}

impl Display {
    pub fn as_str(self) -> &'static str {
        match self {
            Display::Idle => "idle",
            Display::Working => "working",
            Display::Done => "done",
            Display::NeedsInput => "needs-input",
        }
    }
}

impl fmt::Display for Display {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Event::*;

    const fn st(lp: Loop, asks: bool, bg: u32) -> Option<PaneState> {
        Some(PaneState { lp, asks, bg })
    }

    fn disp(s: Option<PaneState>) -> Option<Display> {
        s.map(|s| s.display())
    }

    #[test]
    fn claim_and_reset_start_idle() {
        let busy = st(Loop::Working, true, 2);
        assert_eq!(apply(busy, Claim, false), Some(PaneState::IDLE));
        assert_eq!(apply(busy, Reset, false), Some(PaneState::IDLE));
        assert_eq!(apply(None, Claim, false), Some(PaneState::IDLE));
    }

    #[test]
    fn reclaim_keeps_state() {
        let busy = st(Loop::Working, true, 2);
        assert_eq!(apply(busy, Reclaim, false), busy);
    }

    #[test]
    fn report_on_unowned_pane_is_implicit_claim() {
        assert_eq!(apply(None, Working, false), st(Loop::Working, false, 0));
        assert_eq!(apply(None, Ask, false), st(Loop::Idle, true, 0));
    }

    #[test]
    fn working_resets_bg_and_keeps_asks() {
        assert_eq!(
            apply(st(Loop::Working, true, 3), Working, false),
            st(Loop::Working, true, 0)
        );
    }

    #[test]
    fn ask_shows_needs_input_over_any_loop() {
        for lp in [Loop::Idle, Loop::Working, Loop::Done] {
            let s = apply(st(lp, false, 0), Ask, false);
            assert_eq!(disp(s), Some(Display::NeedsInput), "{lp:?}");
        }
    }

    #[test]
    fn resume_clears_asks() {
        assert_eq!(
            apply(st(Loop::Working, true, 0), Resume, false),
            st(Loop::Working, false, 0)
        );
        assert_eq!(
            apply(st(Loop::Done, true, 0), Resume, false),
            st(Loop::Working, false, 0)
        );
        assert_eq!(
            apply(st(Loop::Idle, true, 0), Resume, false),
            st(Loop::Working, false, 0)
        );
        assert_eq!(
            apply(st(Loop::Error, true, 0), Resume, false),
            st(Loop::Error, false, 0)
        );
    }

    #[test]
    fn done_with_background_work_stays_working() {
        assert_eq!(
            apply(st(Loop::Working, false, 0), Done { bg: 2 }, false),
            st(Loop::Working, false, 2)
        );
    }

    #[test]
    fn done_unseen_or_seen_on_arrival() {
        let w = st(Loop::Working, false, 0);
        assert_eq!(apply(w, Done { bg: 0 }, false), st(Loop::Done, false, 0));
        assert_eq!(apply(w, Done { bg: 0 }, true), st(Loop::Idle, false, 0));
    }

    #[test]
    fn done_never_hides_an_open_ask() {
        let s = apply(st(Loop::Working, true, 0), Done { bg: 0 }, true);
        assert_eq!(disp(s), Some(Display::NeedsInput));
    }

    #[test]
    fn error_shows_needs_input_until_focus() {
        let s = apply(st(Loop::Working, false, 0), Error, false);
        assert_eq!(disp(s), Some(Display::NeedsInput));
        assert_eq!(disp(apply(s, Focus, true)), Some(Display::Idle));
    }

    #[test]
    fn cancel_goes_idle_and_clears_asks() {
        assert_eq!(
            apply(st(Loop::Working, true, 1), Cancel, true),
            st(Loop::Idle, false, 1)
        );
    }

    #[test]
    fn focus_marks_done_seen_but_not_open_asks() {
        assert_eq!(
            apply(st(Loop::Done, false, 0), Focus, true),
            st(Loop::Idle, false, 0)
        );
        assert_eq!(
            disp(apply(st(Loop::Working, true, 0), Focus, true)),
            Some(Display::NeedsInput)
        );
        assert_eq!(
            apply(st(Loop::Working, false, 0), Focus, true),
            st(Loop::Working, false, 0)
        );
    }

    #[test]
    fn release_clears() {
        assert_eq!(apply(st(Loop::Done, true, 1), Release, false), None);
    }

    #[test]
    fn rollup_priority() {
        use Display::*;
        assert_eq!(rollup([Working, NeedsInput, Done]), Some(NeedsInput));
        assert_eq!(rollup([Working, Done]), Some(Done));
        assert_eq!(rollup([Idle, Working]), Some(Working));
        assert_eq!(rollup([Idle, Idle]), None);
        assert_eq!(rollup([]), None);
    }

    #[test]
    fn summary_skips_idle() {
        let mut s = Summary::default();
        for d in [
            Display::NeedsInput,
            Display::Idle,
            Display::Working,
            Display::Working,
        ] {
            s.add(d);
        }
        assert_eq!(
            s,
            Summary {
                needs_input: 1,
                done: 0,
                working: 2
            }
        );
    }
}
