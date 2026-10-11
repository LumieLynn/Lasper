// Contains portions adapted from dekit (formerly mprocs).
// https://github.com/pvolok/dekit (MIT)
// Copyright (c) 2022 Pavel Volokitin
#![allow(dead_code)]

use super::color::Color;

const TEXT_MODE_BOLD: u16 = 0b0000_0000_0001;
const TEXT_MODE_ITALIC: u16 = 0b0000_0000_0010;
const TEXT_MODE_UNDERLINE: u16 = 0b0000_0000_0100;
const TEXT_MODE_INVERSE: u16 = 0b0000_0000_1000;
const TEXT_MODE_DIM: u16 = 0b0000_0001_0000;
const TEXT_MODE_HIDDEN: u16 = 0b0000_0010_0000;
const TEXT_MODE_STRIKEOUT: u16 = 0b0000_0100_0000;
const TEXT_MODE_SLOW_BLINK: u16 = 0b0000_1000_0000;
const TEXT_MODE_RAPID_BLINK: u16 = 0b0001_0000_0000;

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Attrs {
    pub fgcolor: Color,
    pub bgcolor: Color,
    pub mode: u16,
}

impl Attrs {
    pub fn fg(&mut self, color: Color) -> Self {
        self.fgcolor = color;
        *self
    }

    pub fn bg(&mut self, color: Color) -> Self {
        self.bgcolor = color;
        *self
    }

    pub fn bold(&self) -> bool {
        self.mode & TEXT_MODE_BOLD != 0
    }

    pub fn set_bold(&mut self, bold: bool) -> &mut Self {
        if bold {
            self.mode |= TEXT_MODE_BOLD;
        } else {
            self.mode &= !TEXT_MODE_BOLD;
        }
        self
    }

    pub fn italic(&self) -> bool {
        self.mode & TEXT_MODE_ITALIC != 0
    }

    pub fn set_italic(&mut self, italic: bool) -> &mut Self {
        if italic {
            self.mode |= TEXT_MODE_ITALIC;
        } else {
            self.mode &= !TEXT_MODE_ITALIC;
        }
        self
    }

    pub fn underline(&self) -> bool {
        self.mode & TEXT_MODE_UNDERLINE != 0
    }

    pub fn set_underline(&mut self, underline: bool) -> &mut Self {
        if underline {
            self.mode |= TEXT_MODE_UNDERLINE;
        } else {
            self.mode &= !TEXT_MODE_UNDERLINE;
        }
        self
    }

    pub fn inverse(&self) -> bool {
        self.mode & TEXT_MODE_INVERSE != 0
    }

    pub fn set_inverse(&mut self, inverse: bool) -> &mut Self {
        if inverse {
            self.mode |= TEXT_MODE_INVERSE;
        } else {
            self.mode &= !TEXT_MODE_INVERSE;
        }
        self
    }

    pub fn dim(&self) -> bool {
        self.mode & TEXT_MODE_DIM != 0
    }

    pub fn set_dim(&mut self, dim: bool) -> &mut Self {
        if dim {
            self.mode |= TEXT_MODE_DIM;
        } else {
            self.mode &= !TEXT_MODE_DIM;
        }
        self
    }

    pub fn hidden(&self) -> bool {
        self.mode & TEXT_MODE_HIDDEN != 0
    }

    pub fn set_hidden(&mut self, hidden: bool) -> &mut Self {
        if hidden {
            self.mode |= TEXT_MODE_HIDDEN;
        } else {
            self.mode &= !TEXT_MODE_HIDDEN;
        }
        self
    }

    pub fn strikeout(&self) -> bool {
        self.mode & TEXT_MODE_STRIKEOUT != 0
    }

    pub fn set_strikeout(&mut self, strikeout: bool) -> &mut Self {
        if strikeout {
            self.mode |= TEXT_MODE_STRIKEOUT;
        } else {
            self.mode &= !TEXT_MODE_STRIKEOUT;
        }
        self
    }

    pub fn slow_blink(&self) -> bool {
        self.mode & TEXT_MODE_SLOW_BLINK != 0
    }

    pub fn set_slow_blink(&mut self, slow_blink: bool) -> &mut Self {
        if slow_blink {
            self.mode |= TEXT_MODE_SLOW_BLINK;
        } else {
            self.mode &= !TEXT_MODE_SLOW_BLINK;
        }
        self
    }

    pub fn rapid_blink(&self) -> bool {
        self.mode & TEXT_MODE_RAPID_BLINK != 0
    }

    pub fn set_rapid_blink(&mut self, rapid_blink: bool) -> &mut Self {
        if rapid_blink {
            self.mode |= TEXT_MODE_RAPID_BLINK;
        } else {
            self.mode &= !TEXT_MODE_RAPID_BLINK;
        }
        self
    }
}
