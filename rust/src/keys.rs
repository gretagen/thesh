//! Shared key codes and modifier bits — port of the enum / `MOD_*`
//! defines in `src/thesh.h`. Values are pinned to the C original because
//! `Bind.key` stores them alongside plain character codes.

// K_NONE + 1 … exact C enum order (thesh.h)
pub const K_NONE: i32 = 256;
pub const K_EOF: i32 = 257;
pub const K_TAB: i32 = 258;
pub const K_ENTER: i32 = 259;
pub const K_DEL: i32 = 260; // backspace
pub const K_FDEL: i32 = 261;
pub const K_UP: i32 = 262;
pub const K_DOWN: i32 = 263;
pub const K_LEFT: i32 = 264;
pub const K_RIGHT: i32 = 265;
pub const K_HOME: i32 = 266;
pub const K_END: i32 = 267;
pub const K_PGUP: i32 = 268;
pub const K_PGDN: i32 = 269;
pub const K_WLEFT: i32 = 270;
pub const K_WRIGHT: i32 = 271;
pub const K_CTL_A: i32 = 272;
pub const K_CTL_E: i32 = 273;
pub const K_CTL_B: i32 = 274;
pub const K_CTL_F: i32 = 275;
pub const K_CTL_K: i32 = 276;
pub const K_CTL_U: i32 = 277;
pub const K_CTL_W: i32 = 278;
pub const K_CTL_L: i32 = 279;
pub const K_CTL_R: i32 = 280;
pub const K_CTL_C: i32 = 281;
pub const K_CTL_D: i32 = 282;
pub const K_F1: i32 = 283;
pub const K_F2: i32 = 284;
pub const K_F3: i32 = 285;
pub const K_F4: i32 = 286;
pub const K_F5: i32 = 287;
pub const K_F6: i32 = 288;
pub const K_F7: i32 = 289;
pub const K_F8: i32 = 290;
pub const K_F9: i32 = 291;
pub const K_F10: i32 = 292;
pub const K_F11: i32 = 293;
pub const K_F12: i32 = 294;
pub const K_F13: i32 = 295;
pub const K_F14: i32 = 296;
pub const K_F15: i32 = 297;
pub const K_F16: i32 = 298;
pub const K_F17: i32 = 299;
pub const K_F18: i32 = 300;
pub const K_F19: i32 = 301;
pub const K_F20: i32 = 302;
pub const K_F21: i32 = 303;
pub const K_F22: i32 = 304;
pub const K_F23: i32 = 305;
pub const K_F24: i32 = 306;

pub const MOD_CTRL: i32 = 1;
pub const MOD_ALT: i32 = 2;
pub const MOD_SUPER: i32 = 4;
pub const MOD_SHIFT: i32 = 8;
