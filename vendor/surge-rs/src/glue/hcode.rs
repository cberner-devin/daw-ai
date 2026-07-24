/*
 * this file contains hard-coded definitions pulled from the surge source.
 * they're here so that they can be accessed easily.
 * expect more to be handled by bindings as time goes on (just too much of a hassle to manage now).
 */

// constants ripped straight from Surgestorage.h.
#![allow(unused_variables)]
pub const N_OSCS: i32           = 3;
pub const N_LFOS_VOICE: i32     = 6;
pub const N_LFOS_SCENE: i32     = 6;
pub const N_LFOS: i32           = N_LFOS_VOICE + N_LFOS_SCENE;
pub const MAX_LFO_INDICES: i32  = 8;
pub const N_OSC_PARAMS: i32     = 7;
pub const N_EGS: i32            = 2;
pub const N_FX_PARAMS: i32      = 12;
pub const N_FX_SLOTS: i32       = 16;
pub const N_FX_CHAINS: i32      = 4;
pub const N_FX_PER_CHAIN: i32   = 4;
pub const N_SEND_SLOTS: i32     = 4;
pub const FIR_IPOL_M: i32       = 256;
pub const FIR_IPOL_M_BITS: i32  = 8;
pub const FIR_IPOL_N: i32       = 12;
pub const FIR_OFFSET: i32       = FIR_IPOL_N >> 1;
pub const FIR_IPOL_I16_N: i32   = 8;
pub const FIR_OFFSET_I16: i32   = FIR_IPOL_I16_N >> 1;
