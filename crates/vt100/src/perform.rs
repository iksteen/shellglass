const BASE64: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/=";
const CLIPBOARD_SELECTOR: &[u8] = b"cpqs01234567";

pub struct WrappedScreen<CB: crate::callbacks::Callbacks = ()> {
    pub screen: crate::screen::Screen,
    pub callbacks: CB,
}

impl WrappedScreen<()> {
    pub fn new(rows: u16, cols: u16, scrollback_len: usize) -> Self {
        Self::new_with_callbacks(rows, cols, scrollback_len, ())
    }
}

impl<CB: crate::callbacks::Callbacks> WrappedScreen<CB> {
    pub fn new_with_callbacks(
        rows: u16,
        cols: u16,
        scrollback_len: usize,
        callbacks: CB,
    ) -> Self {
        Self {
            screen: crate::screen::Screen::new(
                crate::grid::Size { rows, cols },
                scrollback_len,
            ),
            callbacks,
        }
    }
}

impl<CB: crate::callbacks::Callbacks> vte::Perform for WrappedScreen<CB> {
    fn print(&mut self, c: char) {
        if c == '\u{fffd}' || ('\u{80}'..'\u{a0}').contains(&c) {
            self.callbacks.unhandled_char(&mut self.screen, c);
        } else {
            self.screen.text(c);
        }
    }

    fn execute(&mut self, b: u8) {
        match b {
            7 => self.callbacks.audible_bell(&mut self.screen),
            8 => self.screen.bs(),
            9 => self.screen.tab(),
            10 => self.screen.lf(),
            11 => self.screen.vt(),
            12 => self.screen.ff(),
            13 => self.screen.cr(),
            // we don't implement shift in/out alternate character sets, but
            // it shouldn't count as an "error"
            14 | 15 => {}
            _ => self.callbacks.unhandled_control(&mut self.screen, b),
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, b: u8) {
        if let Some(i) = intermediates.first() {
            self.callbacks.unhandled_escape(
                &mut self.screen,
                Some(*i),
                intermediates.get(1).copied(),
                b,
            );
        } else {
            match b {
                b'7' => self.screen.decsc(),
                b'8' => self.screen.decrc(),
                b'=' => self.screen.deckpam(),
                b'>' => self.screen.deckpnm(),
                b'H' => self.screen.hts(), // shellglass: HTS
                b'M' => self.screen.ri(),
                // shellglass: ST — vte ends an OSC/DCS string itself and then
                // reports the terminator as a bare escape; pure syntax,
                // deliberately ignored (not a fidelity gap).
                b'\\' => {}
                b'c' => self.screen.ris(),
                b'g' => self.callbacks.visual_bell(&mut self.screen),
                _ => {
                    self.callbacks.unhandled_escape(
                        &mut self.screen,
                        None,
                        None,
                        b,
                    );
                }
            }
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        _ignore: bool,
        c: char,
    ) {
        let unhandled = |screen: &mut crate::screen::Screen| {
            self.callbacks.unhandled_csi(
                screen,
                intermediates.first().copied(),
                intermediates.get(1).copied(),
                &params.iter().collect::<Vec<_>>(),
                c,
            );
        };
        match intermediates.first() {
            None => match c {
                '@' => self.screen.ich(canonicalize_params_1(params, 1)),
                'A' => self.screen.cuu(canonicalize_params_1(params, 1)),
                // shellglass: VPR (e) moves like CUD
                'B' | 'e' => {
                    self.screen.cud(canonicalize_params_1(params, 1));
                }
                // shellglass: HPR (a) moves like CUF
                'C' | 'a' => {
                    self.screen.cuf(canonicalize_params_1(params, 1));
                }
                'D' => self.screen.cub(canonicalize_params_1(params, 1)),
                'E' => self.screen.cnl(canonicalize_params_1(params, 1)),
                'F' => self.screen.cpl(canonicalize_params_1(params, 1)),
                // shellglass: HPA (`) moves like CHA
                'G' | '`' => {
                    self.screen.cha(canonicalize_params_1(params, 1));
                }
                // shellglass: HVP (f) is the CUP alias
                'H' | 'f' => {
                    self.screen.cup(canonicalize_params_2(params, 1, 1));
                }
                // shellglass: CHT
                'I' => self.screen.cht(canonicalize_params_1(params, 1)),
                'J' => self
                    .screen
                    .ed(canonicalize_params_1(params, 0), unhandled),
                'K' => self
                    .screen
                    .el(canonicalize_params_1(params, 0), unhandled),
                'L' => self.screen.il(canonicalize_params_1(params, 1)),
                'M' => self.screen.dl(canonicalize_params_1(params, 1)),
                'P' => self.screen.dch(canonicalize_params_1(params, 1)),
                'S' => self.screen.su(canonicalize_params_1(params, 1)),
                'T' => self.screen.sd(canonicalize_params_1(params, 1)),
                'X' => self.screen.ech(canonicalize_params_1(params, 1)),
                // shellglass: CBT
                'Z' => self.screen.cbt(canonicalize_params_1(params, 1)),
                // shellglass: REP
                'b' => self.screen.rep(canonicalize_params_1(params, 1)),
                'd' => self.screen.vpa(canonicalize_params_1(params, 1)),
                // shellglass: Primary DA — an identity query with zero render
                // effect; answering is the embedding terminal's job.
                // Deliberately ignored, not unhandled.
                'c' => {}
                // shellglass: TBC
                'g' => self.screen.tbc(canonicalize_params_1(params, 0)),
                'm' => self.screen.sgr(params, unhandled),
                'r' => self.screen.decstbm(canonicalize_params_decstbm(
                    params,
                    self.screen.grid().size(),
                )),
                // shellglass: SCOSC/SCORC, only in their bare parameterless
                // forms (vte hands a bare CSI a single default-0 param) —
                // `CSI <n> s` would be DECSLRM (unsupported, and its
                // left/right-margin semantics must not save the cursor).
                's' if canonicalize_params_1(params, 0) == 0 => {
                    self.screen.scosc();
                }
                'u' if canonicalize_params_1(params, 0) == 0 => {
                    self.screen.scorc();
                }
                't' => {
                    let mut params_iter = params.iter();
                    let op =
                        params_iter.next().and_then(|x| x.first().copied());
                    match op {
                        Some(8) => {
                            let (screen_rows, screen_cols) =
                                self.screen.size();
                            let rows =
                                params_iter.next().map_or(screen_rows, |x| {
                                    *x.first().unwrap_or(&screen_rows)
                                });
                            let cols =
                                params_iter.next().map_or(screen_cols, |x| {
                                    *x.first().unwrap_or(&screen_cols)
                                });
                            self.callbacks
                                .resize(&mut self.screen, (rows, cols));
                        }
                        // shellglass: XTWINOPS reports (11/13/14/16/18/19/21
                        // are queries the embedding terminal answers) and the
                        // title stack (22/23 — nothing renders a title here)
                        // have zero render effect. Deliberately ignored, not
                        // unhandled; ops outside this set keep reporting.
                        Some(11 | 13 | 14 | 16 | 18 | 19 | 21 | 22 | 23) => {}
                        _ => {
                            self.callbacks.unhandled_csi(
                                &mut self.screen,
                                None,
                                None,
                                &params.iter().collect::<Vec<_>>(),
                                c,
                            );
                        }
                    }
                }
                _ => {
                    self.callbacks.unhandled_csi(
                        &mut self.screen,
                        None,
                        None,
                        &params.iter().collect::<Vec<_>>(),
                        c,
                    );
                }
            },
            Some(b'?') => match c {
                'J' => self
                    .screen
                    .decsed(canonicalize_params_1(params, 0), unhandled),
                'K' => self
                    .screen
                    .decsel(canonicalize_params_1(params, 0), unhandled),
                'h' => self.screen.decset(params, unhandled),
                'l' => self.screen.decrst(params, unhandled),
                _ => {
                    self.callbacks.unhandled_csi(
                        &mut self.screen,
                        Some(b'?'),
                        intermediates.get(1).copied(),
                        &params.iter().collect::<Vec<_>>(),
                        c,
                    );
                }
            },
            // shellglass: Secondary DA (`CSI > c`) — same identity-query
            // family as Primary DA above; deliberately ignored.
            Some(b'>') => match c {
                'c' => {}
                _ => {
                    self.callbacks.unhandled_csi(
                        &mut self.screen,
                        Some(b'>'),
                        intermediates.get(1).copied(),
                        &params.iter().collect::<Vec<_>>(),
                        c,
                    );
                }
            },
            // shellglass: intermediate `!`
            Some(b'!') => match c {
                // DECSTR, soft terminal reset
                'p' => self.screen.decstr(),
                _ => {
                    self.callbacks.unhandled_csi(
                        &mut self.screen,
                        Some(b'!'),
                        intermediates.get(1).copied(),
                        &params.iter().collect::<Vec<_>>(),
                        c,
                    );
                }
            },
            Some(i) => {
                self.callbacks.unhandled_csi(
                    &mut self.screen,
                    Some(*i),
                    intermediates.get(1).copied(),
                    &params.iter().collect::<Vec<_>>(),
                    c,
                );
            }
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bel_terminated: bool) {
        match params {
            [b"0", s] => {
                self.callbacks.set_window_icon_name(&mut self.screen, s);
                self.callbacks.set_window_title(&mut self.screen, s);
            }
            [b"1", s] => {
                self.callbacks.set_window_icon_name(&mut self.screen, s);
            }
            [b"2", s] => {
                self.callbacks.set_window_title(&mut self.screen, s);
            }
            // shellglass: default fg/bg *queries* (vim/neovim background
            // detection) — the embedding terminal answers; nothing to render.
            // The set form (a color value) must stay unhandled until it is
            // mirrored (roadmap item 13): it really changes the local screen.
            [b"10" | b"11", b"?"] => {}
            [b"52", ty, data] => {
                match (
                    ty.iter().all(|c| CLIPBOARD_SELECTOR.contains(c)),
                    *data,
                ) {
                    (true, b"?") => {
                        self.callbacks
                            .paste_from_clipboard(&mut self.screen, ty);
                    }
                    (true, data)
                        if data.iter().all(|c| BASE64.contains(c)) =>
                    {
                        self.callbacks.copy_to_clipboard(
                            &mut self.screen,
                            ty,
                            data,
                        );
                    }
                    _ => {
                        self.callbacks
                            .unhandled_osc(&mut self.screen, params);
                    }
                }
            }
            _ => {
                self.callbacks.unhandled_osc(&mut self.screen, params);
            }
        }
    }
}

fn canonicalize_params_1(params: &vte::Params, default: u16) -> u16 {
    let first = params.iter().next().map_or(0, |x| *x.first().unwrap_or(&0));
    if first == 0 {
        default
    } else {
        first
    }
}

fn canonicalize_params_2(
    params: &vte::Params,
    default1: u16,
    default2: u16,
) -> (u16, u16) {
    let mut iter = params.iter();
    let first = iter.next().map_or(0, |x| *x.first().unwrap_or(&0));
    let first = if first == 0 { default1 } else { first };

    let second = iter.next().map_or(0, |x| *x.first().unwrap_or(&0));
    let second = if second == 0 { default2 } else { second };

    (first, second)
}

fn canonicalize_params_decstbm(
    params: &vte::Params,
    size: crate::grid::Size,
) -> (u16, u16) {
    let mut iter = params.iter();
    let top = iter.next().map_or(0, |x| *x.first().unwrap_or(&0));
    let top = if top == 0 { 1 } else { top };

    let bottom = iter.next().map_or(0, |x| *x.first().unwrap_or(&0));
    let bottom = if bottom == 0 { size.rows } else { bottom };

    (top, bottom)
}
