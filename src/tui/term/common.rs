// Ported from mprocs <https://github.com/pvolok/mprocs> (MIT)
// Copyright (c) 2022 Pavel Volokitin

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Size {
    pub height: u16,
    pub width: u16,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum CursorStyle {
    #[default]
    Default = 0,
    BlinkingBlock = 1,
    SteadyBlock = 2,
    BlinkingUnderline = 3,
    SteadyUnderline = 4,
    BlinkingBar = 5,
    SteadyBar = 6,
}
