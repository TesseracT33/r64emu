extern crate emu;

use super::decode::{decode, ACC_NAMES, VREG_NAMES};
use super::sp::Sp;
use super::vclip;
use super::vmul;
use super::vrcp;

use crate::errors::*;
use byteorder::{BigEndian, ByteOrder, LittleEndian};
use emu::bus::be::{Bus, Device};
use emu::dbg;
use emu::int::Numerics;
use emu::memint::MemInt;
use emu::state::Field;
use mips64::{Cop, CpuContext};
use serde_derive::{Deserialize, Serialize};
use slog;
use std::arch::x86_64::*;

// Vector registers as array of u8.
// Kept as little endian so that it's easier to directly load into SSE registers
#[derive(Debug, Default, Copy, Clone, Serialize, Deserialize)]
#[repr(align(16))]
struct VectorReg([u8; 16]);

impl VectorReg {
    fn byte(&self, idx: usize) -> u8 {
        self.0[15 - idx]
    }
    fn setbyte(&mut self, idx: usize, val: u8) {
        self.0[15 - idx] = val;
    }

    fn lane(&self, idx: usize) -> u16 {
        LittleEndian::read_u16(&self.0[(7 - idx) * 2..])
    }
    fn setlane(&mut self, idx: usize, val: u16) {
        LittleEndian::write_u16(&mut self.0[(7 - idx) * 2..], val);
    }

    fn u128(&self) -> u128 {
        LittleEndian::read_u128(&self.0)
    }
    fn setu128(&mut self, val: u128) {
        LittleEndian::write_u128(&mut self.0, val);
    }

    fn m128(&self) -> __m128i {
        unsafe { _mm_loadu_si128(self.0.as_ptr() as *const _) }
    }
    fn setm128(&mut self, val: __m128i) {
        unsafe { _mm_store_si128(self.0.as_ptr() as *mut _, val) };
    }
}

#[derive(Copy, Clone, Default, Serialize, Deserialize)]
struct SpCop2Context {
    vregs: [VectorReg; 32],
    accum: [VectorReg; 3],
    vco_carry: VectorReg,
    vco_ne: VectorReg,
    vce: VectorReg,
    vcc_normal: VectorReg,
    vcc_clip: VectorReg,
    div_in: Option<u32>,
    div_out: u32,
}

pub struct SpCop2 {
    ctx: Field<SpCop2Context>,
    name: String,
    logger: slog::Logger,
}

impl SpCop2 {
    pub const REG_VCO: usize = 32;
    pub const REG_VCC: usize = 33;
    pub const REG_VCE: usize = 34;
    pub const REG_ACCUM_LO: usize = 35;
    pub const REG_ACCUM_MD: usize = 36;
    pub const REG_ACCUM_HI: usize = 37;

    pub fn new(name: &str, logger: slog::Logger) -> Result<SpCop2> {
        Ok(SpCop2 {
            name: name.to_owned(),
            ctx: Field::new("sp::cop2", SpCop2Context::default()),
            logger: logger,
        })
    }

    fn oploadstore(op: u32, ctx: &CpuContext) -> (u32, usize, u32, u32, u32) {
        let base = ctx.regs[((op >> 21) & 0x1F) as usize] as u32;
        let vt = ((op >> 16) & 0x1F) as usize;
        let opcode = (op >> 11) & 0x1F;
        let element = (op >> 7) & 0xF;
        let offset = (((op as i32) & 0x7F) << 25) >> 25;
        (base, vt, opcode, element, offset as u32)
    }
}

impl SpCop2Context {
    fn vce(&self) -> u8 {
        let mut res = 0u8;
        for i in 0..8 {
            res |= ((self.vce.lane(i) & 1) << i) as u8;
        }
        res
    }
    fn set_vce(&mut self, vce: u8) {
        for i in 0..8 {
            let vce = (vce >> i) & 1;
            self.vce.setlane(i, if vce != 0 { 0xFFFF } else { 0 });
        }
    }

    fn vcc(&self) -> u16 {
        let mut res = 0u16;
        for i in 0..8 {
            res |= (self.vcc_normal.lane(i) & 1) << i;
            res |= (self.vcc_clip.lane(i) & 1) << (i + 8);
        }
        res
    }
    fn set_vcc(&mut self, vcc: u16) {
        for i in 0..8 {
            let normal = (vcc >> i) & 1;
            let clip = (vcc >> (i + 8)) & 1;

            self.vcc_normal
                .setlane(i, if normal != 0 { 0xFFFF } else { 0 });
            self.vcc_clip.setlane(i, if clip != 0 { 0xFFFF } else { 0 });
        }
    }

    fn vco(&self) -> u16 {
        let mut res = 0u16;
        for i in 0..8 {
            res |= (self.vco_carry.lane(i) & 1) << i;
            res |= (self.vco_ne.lane(i) & 1) << (i + 8);
        }
        res
    }

    fn set_vco(&mut self, vco: u16) {
        for i in 0..8 {
            let carry = (vco >> i) & 1;
            let ne = (vco >> (i + 8)) & 1;

            self.vco_carry
                .setlane(i, if carry != 0 { 0xFFFF } else { 0 });
            self.vco_ne.setlane(i, if ne != 0 { 0xFFFF } else { 0 });
        }
    }
}

struct Vectorop<'a> {
    op: u32,
    ctx: &'a mut SpCop2Context,
    spv: &'a mut SpCop2,
}

impl<'a> Vectorop<'a> {
    fn func(&self) -> u32 {
        self.op & 0x3F
    }
    fn e(&self) -> usize {
        ((self.op >> 21) & 0xF) as usize
    }
    fn rs(&self) -> usize {
        ((self.op >> 11) & 0x1F) as usize
    }
    fn rt(&self) -> usize {
        ((self.op >> 16) & 0x1F) as usize
    }
    fn rd(&self) -> usize {
        ((self.op >> 6) & 0x1F) as usize
    }
    fn vs(&self) -> __m128i {
        self.ctx.vregs[self.rs()].m128()
    }
    fn vt(&self) -> __m128i {
        self.ctx.vregs[self.rt()].m128()
    }
    unsafe fn vte(&self) -> __m128i {
        let vt = self.ctx.vregs[self.rt()];
        let e = self.e();
        match e {
            0..=1 => vt.m128(),
            2 => _mm_shufflehi_epi16(_mm_shufflelo_epi16(vt.m128(), 0b11_11_01_01), 0b11_11_01_01),
            3 => _mm_shufflehi_epi16(_mm_shufflelo_epi16(vt.m128(), 0b10_10_00_00), 0b10_10_00_00),
            4 => _mm_shufflehi_epi16(_mm_shufflelo_epi16(vt.m128(), 0b11_11_11_11), 0b11_11_11_11),
            5 => _mm_shufflehi_epi16(_mm_shufflelo_epi16(vt.m128(), 0b10_10_10_10), 0b10_10_10_10),
            6 => _mm_shufflehi_epi16(_mm_shufflelo_epi16(vt.m128(), 0b01_01_01_01), 0b01_01_01_01),
            7 => _mm_shufflehi_epi16(_mm_shufflelo_epi16(vt.m128(), 0b00_00_00_00), 0b00_00_00_00),
            8..=15 => _mm_set1_epi16(vt.lane(e - 8) as i16),
            _ => unreachable!(),
        }
    }
    fn setvd(&mut self, val: __m128i) {
        self.ctx.vregs[self.rd()].setm128(val);
    }
    fn accum(&self, idx: usize) -> __m128i {
        unsafe { _mm_loadu_si128(self.ctx.accum[idx].0.as_ptr() as *const _) }
    }
    fn setaccum(&mut self, idx: usize, val: __m128i) {
        unsafe { _mm_store_si128(self.ctx.accum[idx].0.as_ptr() as *mut _, val) }
    }
    fn carry(&self) -> __m128i {
        self.ctx.vco_carry.m128()
    }
    fn setcarry(&mut self, val: __m128i) {
        self.ctx.vco_carry.setm128(val);
    }
    fn ne(&self) -> __m128i {
        self.ctx.vco_ne.m128()
    }
    fn setne(&mut self, val: __m128i) {
        self.ctx.vco_ne.setm128(val);
    }

    fn vce(&self) -> __m128i {
        self.ctx.vce.m128()
    }
    fn setvce(&mut self, val: __m128i) {
        self.ctx.vce.setm128(val);
    }

    fn vccnormal(&self) -> __m128i {
        self.ctx.vcc_normal.m128()
    }
    fn setvccnormal(&mut self, val: __m128i) {
        self.ctx.vcc_normal.setm128(val);
    }

    fn vccclip(&self) -> __m128i {
        self.ctx.vcc_clip.m128()
    }
    fn setvccclip(&mut self, val: __m128i) {
        self.ctx.vcc_clip.setm128(val);
    }

    fn vt_lane(&self, idx: usize) -> u16 {
        self.ctx.vregs[self.rt()].lane(idx)
    }

    fn setvd_lane(&mut self, idx: usize, val: u16) {
        self.ctx.vregs[self.rd()].setlane(idx, val);
    }
    fn setvs_lane(&mut self, idx: usize, val: u16) {
        self.ctx.vregs[self.rs()].setlane(idx, val);
    }

    fn vs_byte(&mut self, idx: usize) -> u8 {
        self.ctx.vregs[self.rs()].byte(idx)
    }
    fn setvs_byte(&mut self, idx: usize, val: u8) {
        self.ctx.vregs[self.rs()].setbyte(idx, val);
    }
}

macro_rules! op_vmul {
    ($op:expr, $name:ident) => {{
        let (res, acc_lo, acc_md, acc_hi) = vmul::$name(
            $op.vs(),
            $op.vte(),
            $op.accum(0),
            $op.accum(1),
            $op.accum(2),
        );
        $op.setvd(res);
        $op.setaccum(0, acc_lo);
        $op.setaccum(1, acc_md);
        $op.setaccum(2, acc_hi);
    }};
}

impl SpCop2 {
    #[target_feature(enable = "sse2")]
    unsafe fn uop(&mut self, cpu: &mut CpuContext, op: u32, t: &dbg::Tracer) -> dbg::Result<()> {
        let mut op = Vectorop {
            op,
            ctx: unsafe { self.ctx.as_mut() },
            spv: self,
        };
        let vzero = _mm_setzero_si128();
        #[allow(overflowing_literals)]
        let vones = _mm_set1_epi16(0xFFFF);

        if op.op & (1 << 25) != 0 {
            match op.func() {
                0x00 => op_vmul!(op, vmulf), // VMULF
                0x01 => op_vmul!(op, vmulu), // VMULU
                0x04 => op_vmul!(op, vmudl), // VMUDL
                0x05 => op_vmul!(op, vmudm), // VMUDM
                0x06 => op_vmul!(op, vmudn), // VMUDN
                0x07 => op_vmul!(op, vmudh), // VMUDH
                0x08 => op_vmul!(op, vmacf), // VMACF
                0x09 => op_vmul!(op, vmacu), // VMACU
                0x0C => op_vmul!(op, vmadl), // VMADL
                0x0D => op_vmul!(op, vmadm), // VMADM
                0x0E => op_vmul!(op, vmadn), // VMADN
                0x0F => op_vmul!(op, vmadh), // VMADH
                0x10 => {
                    // VADD
                    let vs = op.vs();
                    let vt = op.vte();
                    let carry = op.carry();

                    // We need to compute Saturate(VS+VT+CARRY).
                    // Add the carry to the minimum value, as we need to
                    // saturate the final result and not only intermediate
                    // results:
                    //     0x8000 + 0x8000 + 0x1 must be 0x8000, not 0x8001
                    // NOTE: the carry register is either 0x0 or 0xFFFF (-1), so add/sub
                    // operations are reversed.
                    let min = _mm_min_epi16(vs, vt);
                    let max = _mm_max_epi16(vs, vt);
                    op.setvd(_mm_adds_epi16(_mm_subs_epi16(min, carry), max));
                    op.setaccum(0, _mm_sub_epi16(_mm_add_epi16(vs, vt), carry));
                    op.setcarry(vzero);
                    op.setne(vzero);
                }
                0x11 => {
                    // VSUB
                    let vs = op.vs();
                    let vt = op.vte();
                    let carry = op.carry();

                    // We need to compute Saturate(VS-VT-CARRY).
                    // Compute VS-(VT+CARRY), and fix the result if there
                    // was an overflow.
                    // NOTE: the carry register is either 0x0 or 0xFFFF (-1), so add/sub
                    // operations are reversed.
                    let diff = _mm_sub_epi16(vt, carry);
                    let sdiff = _mm_subs_epi16(vt, carry);
                    let mask = _mm_cmpgt_epi16(sdiff, diff);

                    op.setvd(_mm_adds_epi16(_mm_subs_epi16(vs, sdiff), mask));
                    op.setaccum(0, _mm_sub_epi16(vs, diff));
                    op.setcarry(vzero);
                    op.setne(vzero);
                }
                0x13 => {
                    // VABS
                    let vs = op.vs();
                    let vt = op.vte();
                    let res = _mm_sign_epi16(vt, vs);
                    op.setaccum(0, res);
                    op.setvd(res);
                }
                0x14 => {
                    // VADDC
                    let vs = op.vs();
                    let vt = op.vte();
                    let res = _mm_add_epi16(vs, vt);
                    op.setvd(res);
                    op.setaccum(0, res);
                    op.setne(vzero);
                    op.setcarry(_mm_xor_si128(
                        vones,
                        _mm_cmpeq_epi16(res, _mm_adds_epu16(vs, vt)),
                    ));
                }
                0x15 => {
                    // VSUBC
                    let vs = op.vs();
                    let vt = op.vte();
                    let res = _mm_sub_epi16(vs, vt);
                    op.setvd(res);
                    op.setaccum(0, res);

                    #[allow(overflowing_literals)]
                    let mask = _mm_set1_epi16(0x8000);
                    op.setcarry(_mm_cmpgt_epi16(
                        _mm_xor_si128(mask, vt),
                        _mm_xor_si128(mask, vs),
                    ));
                    op.setne(_mm_xor_si128(_mm_cmpeq_epi16(vs, vt), vones));
                }
                0x17 => {
                    // VSUBB -- undocumented?
                    let vs = op.vs();
                    let vt = op.vte();
                    let res = _mm_add_epi16(vs, vt);
                    op.setvd(vzero);
                    op.setaccum(0, res);
                }
                0x19 => {
                    // VSUCB -- undocumented?
                    let vs = op.vs();
                    let vt = op.vte();
                    let res = _mm_add_epi16(vs, vt);
                    op.setvd(vzero);
                    op.setaccum(0, res);
                }
                0x1D => {
                    // VSAR
                    let e = op.e();
                    match e {
                        0..=2 => {
                            op.setvd(vzero);
                        }
                        8..=10 => {
                            // NOTE: VSAR is not able to write the accumulator,
                            // contrary to what documentation says.
                            let sar = op.accum(2 - (e - 8));
                            op.setvd(sar);
                        }
                        _ => unimplemented!(),
                    }
                }
                0x20 => {
                    // VLT
                    let vs = op.vs();
                    let vt = op.vte();
                    let vcc = _mm_or_si128(
                        _mm_cmpgt_epi16(vt, vs),
                        _mm_and_si128(op.ne(), _mm_and_si128(op.carry(), _mm_cmpeq_epi16(vs, vt))),
                    );
                    let res = _mm_or_si128(_mm_and_si128(vcc, vs), _mm_andnot_si128(vcc, vt));
                    op.setaccum(0, res);
                    op.setvd(res);
                    op.setvccnormal(vcc);
                    op.setvccclip(vzero);
                    op.setcarry(vzero);
                    op.setne(vzero);
                }
                0x21 => {
                    // VEQ
                    let vs = op.vs();
                    let vt = op.vte();
                    let vcc = _mm_andnot_si128(op.ne(), _mm_cmpeq_epi16(vs, vt));
                    let res = _mm_or_si128(_mm_and_si128(vcc, vs), _mm_andnot_si128(vcc, vt));

                    op.setvccnormal(vcc);
                    op.setvccclip(vzero);
                    op.setaccum(0, res);
                    op.setvd(res);
                    op.setcarry(vzero);
                    op.setne(vzero);
                }
                0x22 => {
                    // VNE
                    let vs = op.vs();
                    let vt = op.vte();

                    let vcc = _mm_or_si128(
                        _mm_or_si128(_mm_cmpgt_epi16(vt, vs), _mm_cmpgt_epi16(vs, vt)),
                        _mm_and_si128(op.ne(), _mm_cmpeq_epi16(vs, vt)),
                    );
                    let res =
                        _mm_or_si128(_mm_and_si128(vcc, op.vs()), _mm_andnot_si128(vcc, op.vt()));

                    op.setvccnormal(vcc);
                    op.setvccclip(vzero);
                    op.setaccum(0, res);
                    op.setvd(res);
                    op.setcarry(vzero);
                    op.setne(vzero);
                }
                0x23 => {
                    // VGE
                    let vs = op.vs();
                    let vt = op.vte();
                    let vcc = _mm_or_si128(
                        _mm_cmpgt_epi16(vs, vt),
                        _mm_andnot_si128(
                            _mm_and_si128(op.carry(), op.ne()),
                            _mm_cmpeq_epi16(vs, vt),
                        ),
                    );
                    let res = _mm_or_si128(_mm_and_si128(vcc, vs), _mm_andnot_si128(vcc, vt));
                    op.setvccnormal(vcc);
                    op.setvccclip(vzero);
                    op.setaccum(0, res);
                    op.setvd(res);
                    op.setcarry(vzero);
                    op.setne(vzero);
                }
                0x24 => {
                    // VCL
                    let (res, carry, ne, le, ge, vce) = vclip::vcl(
                        op.vs(),
                        op.vte(),
                        op.carry(),
                        op.ne(),
                        op.vccnormal(),
                        op.vccclip(),
                        op.vce(),
                    );
                    op.setvd(res);
                    op.setaccum(0, res);
                    op.setvccnormal(le);
                    op.setvccclip(ge);
                    op.setvce(vce); // always zero
                    op.setcarry(carry); // always zero
                    op.setne(ne); // always zero
                }
                0x25 => {
                    // VCH
                    let (res, carry, ne, le, ge, vce) = vclip::vch(op.vs(), op.vte());
                    op.setvd(res);
                    op.setaccum(0, res);
                    op.setvccnormal(le);
                    op.setvccclip(ge);
                    op.setvce(vce);
                    op.setcarry(carry);
                    op.setne(ne);
                }
                0x26 => {
                    // VCR
                    let (res, carry, ne, le, ge, vce) = vclip::vcr(op.vs(), op.vte());
                    op.setvd(res);
                    op.setaccum(0, res);
                    op.setvccnormal(le);
                    op.setvccclip(ge);
                    op.setvce(vce); // always zero
                    op.setcarry(carry); // always zero
                    op.setne(ne); // always zero
                }
                0x27 => {
                    // VMRG
                    let vs = op.vs();
                    let vt = op.vte();
                    let vcc = op.vccnormal();

                    let res = _mm_or_si128(_mm_and_si128(vcc, vs), _mm_andnot_si128(vcc, vt));
                    op.setvd(res);
                    op.setaccum(0, res);
                    op.setne(vzero);
                    op.setcarry(vzero);
                }
                0x28 => {
                    // VAND
                    let res = _mm_and_si128(op.vs(), op.vte());
                    op.setvd(res);
                    op.setaccum(0, res);
                }
                0x29 => {
                    // VNAND
                    let res = _mm_xor_si128(_mm_and_si128(op.vs(), op.vte()), vones);
                    op.setvd(res);
                    op.setaccum(0, res);
                }
                0x2A => {
                    // VOR
                    let res = _mm_or_si128(op.vs(), op.vte());
                    op.setvd(res);
                    op.setaccum(0, res);
                }
                0x2B => {
                    // VNOR
                    let res = _mm_xor_si128(_mm_or_si128(op.vs(), op.vte()), vones);
                    op.setvd(res);
                    op.setaccum(0, res);
                }
                0x2C => {
                    // VXOR
                    let res = _mm_xor_si128(op.vs(), op.vte());
                    op.setvd(res);
                    op.setaccum(0, res);
                }
                0x2D => {
                    // VNXOR
                    let res = _mm_xor_si128(_mm_xor_si128(op.vs(), op.vte()), vones);
                    op.setvd(res);
                    op.setaccum(0, res);
                }
                0x30 => {
                    // VRCP
                    let x = op.vt_lane(op.e() & 7);
                    let res = vrcp::vrcp(x.sx32());
                    op.setvd_lane(op.rs() & 7, res as u16);
                    op.setaccum(0, op.vt());
                    op.ctx.div_out = res;
                }
                0x31 => {
                    // VRCPL
                    let x = op.vt_lane(op.e() & 7);
                    let res = match op.ctx.div_in {
                        Some(div_in) => vrcp::vrcp((x as u32) | div_in),
                        None => vrcp::vrcp(x.sx32()),
                    };
                    op.setvd_lane(op.rs() & 7, res as u16);
                    op.setaccum(0, op.vt());
                    op.ctx.div_out = res;
                    op.ctx.div_in = None;
                }
                0x32 => {
                    // VRCPH
                    let x = op.vt_lane(op.e() & 7);
                    op.setvd_lane(op.rs() & 7, (op.ctx.div_out >> 16) as u16);
                    op.setaccum(0, op.vt());
                    op.ctx.div_in = Some((x as u32) << 16);
                }
                0x33 => {
                    // VMOV
                    let se = match op.e() {
                        0..=1 => (op.e() & 0b000) | (op.rs() & 0b111),
                        2..=3 => (op.e() & 0b001) | (op.rs() & 0b110),
                        4..=7 => (op.e() & 0b011) | (op.rs() & 0b100),
                        8..=15 => (op.e() & 0b111) | (op.rs() & 0b000),
                        _ => unreachable!(),
                    };

                    let res = op.vt_lane(se);
                    op.setvd_lane(op.rs() & 7, res);
                    // FIXME: update ACCUM with VMOV?
                    op.setaccum(0, op.vt());
                }
                0x34 => {
                    // VRSQ
                    let x = op.vt_lane(op.e() & 7);
                    let res = vrcp::vrsq(x.sx32());
                    op.setvd_lane(op.rs() & 7, res as u16);
                    op.setaccum(0, op.vt());
                    op.ctx.div_out = res;
                }
                0x35 => {
                    // VRSQL
                    let x = op.vt_lane(op.e() & 7);
                    let res = match op.ctx.div_in {
                        Some(div_in) => vrcp::vrsq((x as u32) | div_in),
                        None => vrcp::vrsq(x.sx32()),
                    };
                    op.setvd_lane(op.rs() & 7, res as u16);
                    op.setaccum(0, op.vt());
                    op.ctx.div_out = res;
                    op.ctx.div_in = None;
                }
                0x36 => {
                    // VRSQH
                    let x = op.vt_lane(op.e() & 7);
                    op.setvd_lane(op.rs() & 7, (op.ctx.div_out >> 16) as u16);
                    op.setaccum(0, op.vt());
                    op.ctx.div_in = Some((x as u32) << 16);
                }
                0x37 => {} // VNOP
                0x3f => {} // VNULL

                _ => panic!("unimplemented COP2 VU opcode={}", op.func().hex()),
            }
        } else {
            match op.e() {
                0x0 => {
                    // MFC2
                    let e = op.rd() >> 1;

                    let mut val = (op.vs_byte(e) as u16) << 8;
                    val |= op.vs_byte((e + 1) & 15) as u16;
                    cpu.regs[op.rt()] = val.sx64();
                }
                0x2 => match op.rs() {
                    // CFC2
                    0 => cpu.regs[op.rt()] = op.ctx.vco().sx64(),
                    1 => cpu.regs[op.rt()] = op.ctx.vcc().sx64(),
                    2 => cpu.regs[op.rt()] = op.ctx.vce() as u64,
                    _ => panic!("unimplement COP2 CFC2 reg:{}", op.rs()),
                },
                0x4 => {
                    // MTC2
                    let e = op.rd() >> 1;
                    op.setvs_byte(e, (cpu.regs[op.rt()] >> 8) as u8);
                    if e != 15 {
                        op.setvs_byte(e + 1, cpu.regs[op.rt()] as u8);
                    }
                }
                0x6 => match op.rs() {
                    // CTC2
                    0 => op.ctx.set_vco(cpu.regs[op.rt()] as u16),
                    1 => op.ctx.set_vcc(cpu.regs[op.rt()] as u16),
                    2 => op.ctx.set_vce(cpu.regs[op.rt()] as u8),
                    _ => panic!("unimplement COP2 CTC2 reg:{}", op.rd()),
                },
                _ => {
                    error!(
                        op.spv.logger,
                        "unimplemented COP2 non-VU opcode={:x}",
                        op.e()
                    );
                    return t.break_here("unimplemented COP2 non-VU opcode");
                }
            }
        }
        Ok(())
    }
}

fn write_partial_left<B: ByteOrder>(dst: &mut [u8], src: u128, skip_bits: usize) {
    let mask = !0u128;
    let mask = if skip_bits < 128 {
        mask << skip_bits
    } else {
        0
    };
    let src = if skip_bits < 128 { src << skip_bits } else { 0 };

    let mut d = B::read_u128(dst);
    d = (d & !mask) | (src & mask);
    B::write_u128(dst, d);
}

fn write_partial_right<B: ByteOrder>(dst: &mut [u8], src: u128, skip_bits: usize, nbits: usize) {
    let mask = !0u128;
    let mask = mask << (128 - nbits);
    let mask = if skip_bits < 128 {
        mask >> skip_bits
    } else {
        0
    };
    let src = if skip_bits < 128 { src >> skip_bits } else { 0 };

    let mut d = B::read_u128(dst);
    d = (d & !mask) | (src & mask);
    B::write_u128(dst, d);
}

// Plain "load vector subword from memory"
fn lxv<T: MemInt>(reg: &mut VectorReg, element: usize, dmem: &mut [u8], base: u32, offset: u32) {
    let ea = ((base + (offset << T::SIZE_LOG)) & 0xFFF) as usize;
    if ea + T::SIZE > 0x1000 {
        for i in 0..16 {
            // Mirror the beginning of DMEM after the end (using excess memory that
            // was allocated for this scope).
            dmem[0x1000 + i] = dmem[i];
        }
    }
    let mem64: u64 = T::endian_read_from::<BigEndian>(&dmem[ea..ea + T::SIZE]).into();
    let mut mem: u128 = mem64.into();
    mem <<= 128 - T::SIZE * 8;

    write_partial_right::<LittleEndian>(&mut reg.0, mem, element as usize * 8, T::SIZE * 8);
}

// Plain "store vector subword into memory"
fn sxv<T: MemInt>(dmem: &mut [u8], base: u32, offset: u32, reg: &VectorReg, element: usize) {
    let ea = ((base + (offset << T::SIZE_LOG)) & 0xFFF) as usize;

    let mut reg = reg.u128();
    reg = reg.rotate_left(element as u32 * 8);
    reg >>= 128 - T::SIZE * 8;

    T::endian_write_to::<BigEndian>(&mut dmem[ea..ea + T::SIZE], T::truncate_from(reg as u64));
}

impl Cop for SpCop2 {
    fn reg(&self, _cpu: &CpuContext, idx: usize) -> u128 {
        match idx {
            SpCop2::REG_VCO => self.ctx.vco() as u128,
            SpCop2::REG_VCC => self.ctx.vcc() as u128,
            SpCop2::REG_VCE => self.ctx.vce() as u128,
            SpCop2::REG_ACCUM_LO => LittleEndian::read_u128(&self.ctx.accum[0].0),
            SpCop2::REG_ACCUM_MD => LittleEndian::read_u128(&self.ctx.accum[1].0),
            SpCop2::REG_ACCUM_HI => LittleEndian::read_u128(&self.ctx.accum[2].0),
            _ => self.ctx.vregs[idx].u128(),
        }
    }
    fn set_reg(&mut self, _cpu: &mut CpuContext, idx: usize, val: u128) {
        match idx {
            SpCop2::REG_VCO => self.ctx.set_vco(val as u16),
            SpCop2::REG_VCC => self.ctx.set_vcc(val as u16),
            SpCop2::REG_VCE => self.ctx.set_vce(val as u8),
            SpCop2::REG_ACCUM_LO => LittleEndian::write_u128(&mut self.ctx.accum[0].0, val),
            SpCop2::REG_ACCUM_MD => LittleEndian::write_u128(&mut self.ctx.accum[1].0, val),
            SpCop2::REG_ACCUM_HI => LittleEndian::write_u128(&mut self.ctx.accum[2].0, val),
            _ => self.ctx.vregs[idx].setu128(val),
        }
    }

    fn op(&mut self, cpu: &mut CpuContext, op: u32, t: &dbg::Tracer) -> dbg::Result<()> {
        unsafe { self.uop(cpu, op, t) }
    }

    fn lwc(
        &mut self,
        op: u32,
        ctx: &mut CpuContext,
        _bus: &Bus,
        t: &dbg::Tracer,
    ) -> dbg::Result<()> {
        let sp = Sp::get_mut();
        let mut dmem = &mut sp.dmem;
        let (base, vtidx, op, element, offset) = SpCop2::oploadstore(op, ctx);
        let vt = &mut self.ctx.vregs[vtidx];
        match op {
            0x00 => lxv::<u8>(vt, element as usize, &mut dmem, base, offset), // LBV
            0x01 => lxv::<u16>(vt, element as usize, &mut dmem, base, offset), // LSV
            0x02 => lxv::<u32>(vt, element as usize, &mut dmem, base, offset), // LLV
            0x03 => lxv::<u64>(vt, element as usize, &mut dmem, base, offset), // LDV
            0x04 => {
                // LQV
                let ea = ((base + (offset << 4)) & 0xFFF) as usize;
                let qw_start = ea & !0xF;
                let ea_idx = ea & 0xF;

                let mut mem = BigEndian::read_u128(&dmem[qw_start..qw_start + 0x10]);
                mem <<= ea_idx * 8;
                write_partial_right::<LittleEndian>(&mut vt.0, mem, element as usize * 8, 128);
            }
            0x05 => {
                // LRV
                let ea = ((base + (offset << 4)) & 0xFFF) as usize;
                let qw_start = ea & !0xF;
                let ea_idx = ea & 0xF;

                let mem = BigEndian::read_u128(&dmem[qw_start..qw_start + 0x10]);
                let sh = (16 - ea_idx) + element as usize;
                write_partial_right::<LittleEndian>(&mut vt.0, mem, sh * 8, 128);
            }
            0x06 => {
                // LPV
                let ea = ((base + (offset << 3)) & 0xFFF) as usize;
                let qw_start = ea & !0x7;
                let mut ea_idx = ea & 7;

                ea_idx = (ea_idx - element as usize) & 0xF;
                for e in 0..8 {
                    let mem = dmem[(qw_start + ea_idx) & 0xFFF] as u16;
                    self.ctx.vregs[vtidx].setlane(e, mem << 8);
                    ea_idx += 1;
                    ea_idx &= 0xF;
                }
            }
            0x07 => {
                // LUV
                let ea = ((base + (offset << 3)) & 0xFFF) as usize;
                let qw_start = ea & !0x7;
                let mut ea_idx = ea & 7;

                ea_idx = (ea_idx - element as usize) & 0xF;
                for e in 0..8 {
                    let mem = dmem[(qw_start + ea_idx) & 0xFFF] as u16;
                    self.ctx.vregs[vtidx].setlane(e, mem << 7);
                    ea_idx += 1;
                    ea_idx &= 0xF;
                }
            }
            0x08 => {
                // LHV
                let ea = ((base + (offset << 4)) & 0xFFF) as usize;
                let qw_start = ea & !0x7;
                let mut ea_idx = ea & 0x7;

                ea_idx = (ea_idx - element as usize) & 0xF;
                for e in 0..8 {
                    let mem = dmem[(qw_start + ea_idx) & 0xFFF] as u16;
                    self.ctx.vregs[vtidx].setlane(e, mem << 7);
                    ea_idx += 2;
                    ea_idx &= 0xF;
                }
            }
            0x09 => {
                // LFV
                let ea = ((base + (offset << 4)) & 0xFFF) as usize;
                let qw_start = ea & !0x7;

                let mut high: u64 = 0;
                let mut ea_idx = ((ea & 0x7) - element as usize) & 0xF;
                for e in 0..4 {
                    let mem = dmem[(qw_start + ea_idx) & 0xFFF] as u64;
                    high <<= 16;
                    high |= mem << 7;
                    ea_idx += 4;
                    ea_idx &= 0xF;
                }

                let mut low: u64 = 0;
                let mut ea_idx = ((ea & 0x7) - element as usize + 8) & 0xF;
                for e in 0..4 {
                    let mem = dmem[(qw_start + ea_idx) & 0xFFF] as u64;
                    low <<= 16;
                    low |= mem << 7;
                    ea_idx += 4;
                    ea_idx &= 0xF;
                }

                let new = ((high as u128) << 64) | low as u128;

                let mask: u128 = (0xFFFFFFFFFFFFFFFF0000000000000000) >> (element * 8);
                let r = self.ctx.vregs[vtidx].u128();
                self.ctx.vregs[vtidx].setu128((r & !mask) | (new & mask));
            }
            0x0B => {
                // LTV
                let ea = (base + (offset << 4)) & 0xFFF;
                let qw_start = ea as usize & !0x7;
                let mut mem = if qw_start != 0xFF8 {
                    BigEndian::read_u128(&dmem[qw_start..qw_start + 0x10])
                } else {
                    // Handle wrap around in DMEM
                    ((BigEndian::read_u64(&dmem[0xFF8..0x1000]) as u128) << 64)
                        | BigEndian::read_u64(&dmem[0x0..0x8]) as u128
                };

                let vtbase = vtidx & !7;
                let mut vtoff = element as usize >> 1;
                mem = mem.rotate_left((element + (ea & 0x8)) * 8);

                for e in 0..8 {
                    self.ctx.vregs[vtbase + vtoff].setlane(e, (mem >> (128 - 16)) as u16);
                    mem <<= 16;
                    vtoff += 1;
                    vtoff &= 7;
                }
            }
            _ => return t.panic(&format!("unimplemented VU load opcode={}", op.hex())),
        }
        Ok(())
    }
    fn swc(
        &mut self,
        op: u32,
        ctx: &CpuContext,
        _bus: &mut Bus,
        t: &dbg::Tracer,
    ) -> dbg::Result<()> {
        let sp = Sp::get_mut();
        let mut dmem = &mut sp.dmem;
        let (base, vtidx, op, element, offset) = SpCop2::oploadstore(op, ctx);
        let vt = &self.ctx.vregs[vtidx];
        match op {
            0x00 => sxv::<u8>(&mut dmem, base, offset, vt, element as usize), // SBV
            0x01 => sxv::<u16>(&mut dmem, base, offset, vt, element as usize), // SSV
            0x02 => sxv::<u32>(&mut dmem, base, offset, vt, element as usize), // SLV
            0x03 => sxv::<u64>(&mut dmem, base, offset, vt, element as usize), // SDV
            0x04 => {
                // SQV
                let ea = ((base + (offset << 4)) & 0xFFF) as usize;
                let qw_start = ea & !0xF;
                let ea_idx = ea & 0xF;

                let mut reg = vt.u128();
                reg = reg.rotate_left(element * 8);

                let memptr = &mut dmem[qw_start..qw_start + 0x10];
                write_partial_right::<BigEndian>(memptr, reg, ea_idx * 8, 128);
            }
            0x05 => {
                // SRV
                let ea = ((base + (offset << 4)) & 0xFFF) as usize;
                let qw_start = ea & !0xF;
                let ea_idx = ea & 0xF;

                let mut reg = vt.u128();
                reg = reg.rotate_left(element * 8);

                let memptr = &mut dmem[qw_start..qw_start + 0x10];
                write_partial_left::<BigEndian>(memptr, reg, (16 - ea_idx) * 8);
            }
            0x06 => {
                // SPV
                let ea = ((base + (offset << 3)) & 0xFFF) as usize;

                let memptr = &mut dmem[ea..ea + 0x10];
                for e in 0 as usize..8 as usize {
                    let eidx = (e + element as usize) & 0xF;
                    memptr[e] = ((vt.lane(eidx & 0x7) << (eidx >> 3)) >> 8) as u8;
                }
            }
            0x07 => {
                // SUV
                let ea = ((base + (offset << 3)) & 0xFFF) as usize;

                let memptr = &mut dmem[ea..ea + 0x10];
                for e in 0 as usize..8 as usize {
                    let eidx = (e + element as usize) & 0xF;
                    memptr[e] = ((vt.lane(eidx & 0x7) >> (eidx >> 3)) >> 7) as u8;
                }
            }
            0x08 => {
                // SHV
                let ea = ((base + (offset << 4)) & 0xFFF) as usize;
                let qw_start = ea as usize & !0x7;
                let ea_idx = ea & 0x7;

                let memptr = &mut dmem[qw_start..qw_start + 0x10];
                for e in 0 as usize..8 as usize {
                    let eidx = (e * 2 + element as usize) & 0xF;
                    let midx = (e * 2 + ea_idx) & 0xF;
                    let v = ((vt.byte(eidx) as u16) << 8) | vt.byte((eidx + 1) & 0xF) as u16;
                    memptr[midx] = (v >> 7) as u8;
                }
            }
            0x09 => {
                // SFV
                // FIXME: this is dumped through experimentation. Surely there's no
                // table in the silicon... figure it out the pattern and the logic.
                const LANES: [[isize; 4]; 16] = [
                    [0, 1, 2, 3],     // e0
                    [6, 7, 4, 5],     // e1
                    [-1, -1, -1, -1], // e2
                    [-1, -1, -1, -1], // e3
                    [1, 2, 3, 0],     // e4
                    [7, 4, 5, 6],     // e5
                    [-1, -1, -1, -1], // e6
                    [-1, -1, -1, -1], // e7
                    [4, 5, 6, 7],     // e8
                    [-1, -1, -1, -1], // e9
                    [-1, -1, -1, -1], // e10
                    [3, 0, 1, 2],     // e11
                    [5, 6, 7, 4],     // e12
                    [-1, -1, -1, -1], // e13
                    [-1, -1, -1, -1], // e14
                    [0, 1, 2, 3],     // e15
                ];

                let ea = ((base + (offset << 4)) & 0xFFF) as usize;
                let qw_start = ea as usize & !0x7;
                let ea_idx = ea & 0x7;

                let memptr = &mut dmem[qw_start..qw_start + 0x10];
                for e in 0 as usize..4 as usize {
                    let eidx = LANES[element as usize][e];
                    let v = if eidx < 0 {
                        0 as u16
                    } else {
                        vt.lane(eidx as usize) as u16
                    };
                    let midx = (e * 4 + ea_idx) & 0xF;
                    memptr[midx] = (v >> 7) as u8;
                }
            }
            0x0A => {
                // SWV
                let ea = (base + (offset << 4)) & 0xFFF;
                let qw_start = ea as usize & !0x7;

                let mut reg = vt.u128();
                reg = reg.rotate_right((ea & 7) * 8);
                reg = reg.rotate_left(element * 8);
                BigEndian::write_u128(&mut dmem[qw_start..qw_start + 0x10], reg);
            }
            0x0B => {
                // STV
                let ea = (base + (offset << 4)) & 0xFFF;
                let qw_start = ea as usize & !0x7;
                let mut mem: u128 = 0;

                let vtbase = vtidx & !7;
                let mut vtoff = element as usize >> 1;

                for e in 0..8 {
                    let r = self.ctx.vregs[vtbase + vtoff].lane(e);
                    mem <<= 16;
                    mem |= r as u128;
                    vtoff += 1;
                    vtoff &= 7;
                }

                mem = mem.rotate_right((ea & 7) * 8);
                BigEndian::write_u128(&mut dmem[qw_start..qw_start + 0x10], mem);
            }
            _ => return t.panic(&format!("unimplemented VU store opcode={}", op.hex())),
        }
        Ok(())
    }

    fn ldc(
        &mut self,
        _op: u32,
        _ctx: &mut CpuContext,
        _bus: &Bus,
        _t: &dbg::Tracer,
    ) -> dbg::Result<()> {
        unimplemented!()
    }
    fn sdc(
        &mut self,
        _op: u32,
        _ctx: &CpuContext,
        _bus: &mut Bus,
        _t: &dbg::Tracer,
    ) -> dbg::Result<()> {
        unimplemented!()
    }
    fn decode(&self, opcode: u32, pc: u64) -> dbg::DecodedInsn {
        decode(opcode, pc)
    }

    fn render_debug(&mut self, dr: &dbg::DebuggerRenderer) {
        dr.render_regview(self);
    }
}

impl dbg::RegisterView for SpCop2 {
    const WINDOW_SIZE: [f32; 2] = [180.0, 400.0];
    const COLUMNS: usize = 1;

    fn name(&self) -> &str {
        &self.name
    }

    fn cpu_name(&self) -> &'static str {
        "RSP"
    }

    fn visit_regs<'s, F>(&'s mut self, col: usize, mut visit: F)
    where
        F: for<'a> FnMut(&'a str, dbg::RegisterSize<'a>, Option<&str>),
    {
        use emu::dbg::RegisterSize::*;
        let ctx = unsafe { self.ctx.as_mut() };

        let mut vle: [u16; 8] = [0; 8];
        for i in 0..32 {
            let v: &mut [u16; 8] = unsafe { std::mem::transmute(&mut ctx.vregs[i].0) };
            for j in 0..8 {
                vle[j] = v[7 - j];
            }
            visit(&VREG_NAMES[i], Reg16x8(&mut vle), None);
            for j in 0..8 {
                v[j] = vle[7 - j];
            }
        }

        for i in 0..3 {
            let v: &mut [u16; 8] = unsafe { std::mem::transmute(&mut ctx.accum[i].0) };
            for j in 0..8 {
                vle[j] = v[7 - j];
            }
            visit(ACC_NAMES[i], Reg16x8(&mut vle), None);
            for j in 0..8 {
                v[j] = vle[7 - j];
            }
        }

        let (mut vcc, mut vco, mut vce) = (ctx.vcc(), ctx.vco(), ctx.vce());
        visit("VCC", Reg16(&mut vcc), None);
        visit("VCO", Reg16(&mut vco), None);
        visit("VCE", Reg8(&mut vce), None);
        ctx.set_vcc(vcc);
        ctx.set_vco(vco);
        ctx.set_vce(vce);
    }
}
