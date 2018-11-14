extern crate emu;

use super::sp::Sp;
use super::vmul;
use super::vrcp;

use byteorder::{BigEndian, ByteOrder, LittleEndian};
use emu::bus::be::{Bus, DevPtr};
use emu::bus::MemInt;
use emu::int::Numerics;
use errors::*;
use mips64::{Cop, CpuContext};
use slog;
use std::arch::x86_64::*;
use std::cell::RefCell;
use std::rc::Rc;

// Vector registers as array of u8.
// We define a separate structure for this array to be able
// to specify alignment, since these will be used with SSE intrinsics.
#[repr(align(16))]
struct VectorRegs([[u8; 16]; 32]);

#[derive(Debug, Copy, Clone)]
#[repr(align(16))]
struct VectorReg([u8; 16]);

pub(crate) struct SpCop2 {
    vregs: VectorRegs,
    accum: [VectorReg; 3],
    vco_carry: VectorReg,
    vco_ne: VectorReg,
    div_in: u32,
    div_out: u32,
    sp: DevPtr<Sp>,
    logger: slog::Logger,
}

impl SpCop2 {
    pub const REG_VCO: usize = 32;
    pub const REG_VCC: usize = 33;
    pub const REG_VCE: usize = 34;
    pub const REG_ACCUM_LO: usize = 35;
    pub const REG_ACCUM_MD: usize = 36;
    pub const REG_ACCUM_HI: usize = 37;

    pub fn new(sp: &DevPtr<Sp>, logger: slog::Logger) -> Result<Box<SpCop2>> {
        Ok(Box::new(SpCop2 {
            vregs: VectorRegs([[0u8; 16]; 32]),
            accum: [VectorReg([0u8; 16]); 3],
            vco_carry: VectorReg([0u8; 16]),
            vco_ne: VectorReg([0u8; 16]),
            div_in: 0,
            div_out: 0,
            sp: sp.clone(),
            logger,
        }))
    }

    fn oploadstore(op: u32, ctx: &CpuContext) -> (u32, usize, u32, u32, u32) {
        let base = ctx.regs[((op >> 21) & 0x1F) as usize] as u32;
        let vt = ((op >> 16) & 0x1F) as usize;
        let opcode = (op >> 11) & 0x1F;
        let element = (op >> 7) & 0xF;
        let offset = op & 0x7F;
        (base, vt, opcode, element, offset)
    }

    fn vce(&self) -> u16 {
        0
    }
    fn set_vce(&self, _vec: u16) {}

    fn vcc(&self) -> u16 {
        0
    }
    fn set_vcc(&self, _vec: u16) {}

    fn vco(&self) -> u16 {
        let mut res = 0u16;
        for i in 0..8 {
            res |= LittleEndian::read_u16(&self.vco_carry.0[(7 - i) * 2..]) << i;
            res |= LittleEndian::read_u16(&self.vco_ne.0[(7 - i) * 2..]) << (i + 8);
        }
        res
    }

    fn set_vco(&mut self, vco: u16) {
        for i in 0..8 {
            LittleEndian::write_u16(&mut self.vco_carry.0[(7 - i) * 2..], (vco >> i) & 1);
            LittleEndian::write_u16(&mut self.vco_ne.0[(7 - i) * 2..], (vco >> (i + 8)) & 1);
        }
    }
}

struct Vectorop<'a> {
    op: u32,
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
        unsafe { _mm_loadu_si128(self.spv.vregs.0[self.rs()].as_ptr() as *const _) }
    }
    fn vt(&self) -> __m128i {
        unsafe { _mm_loadu_si128(self.spv.vregs.0[self.rt()].as_ptr() as *const _) }
    }
    unsafe fn vte(&self) -> __m128i {
        let vt = _mm_loadu_si128(self.spv.vregs.0[self.rt()].as_ptr() as *const _);
        let e = self.e();
        match e {
            0..=1 => vt,
            2 => _mm_shufflehi_epi16(_mm_shufflelo_epi16(vt, 0b11_11_01_01), 0b11_11_01_01),
            3 => _mm_shufflehi_epi16(_mm_shufflelo_epi16(vt, 0b10_10_00_00), 0b10_10_00_00),
            4 => _mm_shufflehi_epi16(_mm_shufflelo_epi16(vt, 0b11_11_11_11), 0b11_11_11_11),
            5 => _mm_shufflehi_epi16(_mm_shufflelo_epi16(vt, 0b10_10_10_10), 0b10_10_10_10),
            6 => _mm_shufflehi_epi16(_mm_shufflelo_epi16(vt, 0b01_01_01_01), 0b01_01_01_01),
            7 => _mm_shufflehi_epi16(_mm_shufflelo_epi16(vt, 0b00_00_00_00), 0b00_00_00_00),
            8..=15 => _mm_set1_epi16(LittleEndian::read_u16(
                &self.spv.vregs.0[self.rt()][(15 - e) * 2..],
            ) as i16),
            _ => vt,
        }
    }
    fn setvd(&mut self, val: __m128i) {
        unsafe {
            let rd = self.rd();
            _mm_store_si128(self.spv.vregs.0[rd].as_ptr() as *mut _, val);
        }
    }
    fn accum(&self, idx: usize) -> __m128i {
        unsafe { _mm_loadu_si128(self.spv.accum[idx].0.as_ptr() as *const _) }
    }
    fn setaccum(&mut self, idx: usize, val: __m128i) {
        unsafe { _mm_store_si128(self.spv.accum[idx].0.as_ptr() as *mut _, val) }
    }
    fn carry(&self) -> __m128i {
        unsafe { _mm_loadu_si128(self.spv.vco_carry.0.as_ptr() as *const _) }
    }
    fn setcarry(&self, val: __m128i) {
        unsafe { _mm_store_si128(self.spv.vco_carry.0.as_ptr() as *mut _, val) }
    }
    fn setne(&self, val: __m128i) {
        unsafe { _mm_store_si128(self.spv.vco_ne.0.as_ptr() as *mut _, val) }
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
    unsafe fn uop(&mut self, cpu: &mut CpuContext, op: u32) {
        let mut op = Vectorop { op, spv: self };
        let vzero = _mm_setzero_si128();
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
                    let min = _mm_min_epi16(vs, vt);
                    let max = _mm_max_epi16(vs, vt);
                    op.setvd(_mm_adds_epi16(_mm_adds_epi16(min, carry), max));
                    op.setaccum(0, _mm_add_epi16(_mm_add_epi16(vs, vt), carry));
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
                    let zero = _mm_setzero_si128();
                    let carry = _mm_cmpgt_epi16(carry, zero);

                    let diff = _mm_sub_epi16(vt, carry);
                    let sdiff = _mm_subs_epi16(vt, carry);
                    let mask = _mm_cmpgt_epi16(sdiff, diff);

                    op.setvd(_mm_adds_epi16(_mm_subs_epi16(vs, sdiff), mask));
                    op.setaccum(0, _mm_sub_epi16(vs, diff));
                    op.setcarry(vzero);
                    op.setne(vzero);
                }
                0x14 => {
                    // VADDC
                    let vs = op.vs();
                    let vt = op.vte();
                    let res = _mm_add_epi16(vs, vt);
                    op.setvd(res);
                    op.setaccum(0, res);
                    op.setne(vzero);

                    // We need to compute the carry bit. To do so, we use signed
                    // comparison of 16-bit integers, xoring with 0x8000 to obtain
                    // the unsigned result.
                    #[allow(overflowing_literals)]
                    let mask = _mm_set1_epi16(0x8000);
                    let carry = _mm_cmpgt_epi16(_mm_xor_si128(mask, vs), _mm_xor_si128(mask, res));
                    op.setcarry(_mm_srli_epi16(carry, 15));
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
                    let carry = _mm_cmpgt_epi16(_mm_xor_si128(mask, vt), _mm_xor_si128(mask, vs));
                    op.setcarry(_mm_srli_epi16(carry, 15));
                    op.setne(_mm_add_epi16(_mm_cmpeq_epi16(vs, vt), _mm_set1_epi16(1)));
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
                0x28 => {
                    // VAND
                    let res = _mm_and_si128(op.vs(), op.vte());
                    op.setvd(res);
                    op.setaccum(0, res);
                }
                0x29 => {
                    // VNAND
                    let res = _mm_xor_si128(_mm_and_si128(op.vs(), op.vte()), _mm_set1_epi16(-1));
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
                    let res = _mm_xor_si128(_mm_or_si128(op.vs(), op.vte()), _mm_set1_epi16(-1));
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
                    let res = _mm_xor_si128(_mm_xor_si128(op.vs(), op.vte()), _mm_set1_epi16(-1));
                    op.setvd(res);
                    op.setaccum(0, res);
                }
                0x30 => {
                    // VRCP
                    let se = 7 - (op.e() & 7);
                    let de = 7 - (op.rs() & 7);
                    let x = LittleEndian::read_u16(&op.spv.vregs.0[op.rt()][se * 2..]);
                    let res = vrcp::vrcp(x.sx64() as u32);
                    LittleEndian::write_u16(&mut op.spv.vregs.0[op.rd()][de * 2..], res as u16);
                    op.setaccum(0, op.vt());
                    op.spv.div_out = res;
                }
                0x31 => {
                    // VRCPL
                    let se = 7 - (op.e() & 7);
                    let de = 7 - (op.rs() & 7);
                    let x = LittleEndian::read_u16(&op.spv.vregs.0[op.rt()][se * 2..]);
                    let res = vrcp::vrcp((x as u32) | op.spv.div_in);
                    LittleEndian::write_u16(&mut op.spv.vregs.0[op.rd()][de * 2..], res as u16);
                    op.setaccum(0, op.vt());
                    op.spv.div_out = res;
                }
                0x32 => {
                    // VRCPH
                    let se = 7 - (op.e() & 7);
                    let de = 7 - (op.rs() & 7);
                    let x = LittleEndian::read_u16(&op.spv.vregs.0[op.rt()][se * 2..]);
                    LittleEndian::write_u16(
                        &mut op.spv.vregs.0[op.rd()][de * 2..],
                        (op.spv.div_out >> 16) as u16,
                    );
                    op.setaccum(0, op.vt());
                    op.spv.div_in = (x as u32) << 16;
                }
                0x33 => {
                    // VMOV
                    let de = 7 - (op.rs() & 7);
                    let se = 7 - match op.e() {
                        0..=1 => (op.e() & 0b000) | (op.rs() & 0b111),
                        2..=3 => (op.e() & 0b001) | (op.rs() & 0b110),
                        4..=7 => (op.e() & 0b011) | (op.rs() & 0b100),
                        8..=15 => (op.e() & 0b111) | (op.rs() & 0b000),
                        _ => unreachable!(),
                    };

                    let res = LittleEndian::read_u16(&op.spv.vregs.0[op.rt()][se * 2..]);
                    LittleEndian::write_u16(&mut op.spv.vregs.0[op.rd()][de * 2..], res);
                    // FIXME: update ACCUM with VMOV?
                }
                0x34 => {
                    // VRSQ
                    let se = 7 - (op.e() & 7);
                    let de = 7 - (op.rs() & 7);
                    let x = LittleEndian::read_u16(&op.spv.vregs.0[op.rt()][se * 2..]);
                    let res = vrcp::vrsq(x.sx64() as u32);
                    LittleEndian::write_u16(&mut op.spv.vregs.0[op.rd()][de * 2..], res as u16);
                    op.setaccum(0, op.vt());
                    op.spv.div_out = res;
                }
                0x35 => {
                    // VRSQL
                    let se = 7 - (op.e() & 7);
                    let de = 7 - (op.rs() & 7);
                    let x = LittleEndian::read_u16(&op.spv.vregs.0[op.rt()][se * 2..]);
                    let res = vrcp::vrsq((x as u32) | op.spv.div_in);
                    LittleEndian::write_u16(&mut op.spv.vregs.0[op.rd()][de * 2..], res as u16);
                    op.setaccum(0, op.vt());
                    op.spv.div_out = res;
                }
                0x36 => {
                    // VRSQH
                    let se = 7 - (op.e() & 7);
                    let de = 7 - (op.rs() & 7);
                    let x = LittleEndian::read_u16(&op.spv.vregs.0[op.rt()][se * 2..]);
                    LittleEndian::write_u16(
                        &mut op.spv.vregs.0[op.rd()][de * 2..],
                        (op.spv.div_out >> 16) as u16,
                    );
                    op.setaccum(0, op.vt());
                    op.spv.div_in = (x as u32) << 16;
                }

                _ => panic!("unimplemented COP2 VU opcode={}", op.func().hex()),
            }
        } else {
            match op.e() {
                0x2 => match op.rs() {
                    0 => cpu.regs[op.rt()] = op.spv.vco().sx64(),
                    1 => cpu.regs[op.rt()] = op.spv.vcc().sx64(),
                    2 => cpu.regs[op.rt()] = op.spv.vce().sx64(),
                    _ => panic!("unimplement COP2 CFC2 reg:{}", op.rs()),
                },
                0x6 => match op.rs() {
                    0 => op.spv.set_vco(cpu.regs[op.rt()] as u16),
                    1 => op.spv.set_vcc(cpu.regs[op.rt()] as u16),
                    2 => op.spv.set_vce(cpu.regs[op.rt()] as u16),
                    _ => panic!("unimplement COP2 CTC2 reg:{}", op.rd()),
                },
                _ => panic!("unimplemented COP2 non-VU opcode={:x}", op.e()),
            }
        }
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
    let mask = mask & (!0u128 << nbits);
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
fn lxv<T: MemInt>(regptr: &mut [u8], element: usize, dmem: &[u8], base: u32, offset: u32) {
    let ea = ((base + (offset << T::SIZE_LOG)) & 0xFFF) as usize;
    let mem64: u64 = T::endian_read_from::<BigEndian>(&dmem[ea..ea + T::SIZE]).into();
    let mut mem: u128 = mem64.into();
    mem <<= 128 - T::SIZE * 8;

    write_partial_right::<LittleEndian>(regptr, mem, element as usize * 8, T::SIZE * 8);
}

// Plain "store vector subword into memory"
fn sxv<T: MemInt>(dmem: &mut [u8], base: u32, offset: u32, regptr: &[u8], element: usize) {
    let ea = ((base + (offset << T::SIZE_LOG)) & 0xFFF) as usize;

    let mut reg = LittleEndian::read_u128(regptr);
    reg = reg.rotate_left(element as u32 * 8);
    reg >>= 128 - T::SIZE * 8;

    T::endian_write_to::<BigEndian>(&mut dmem[ea..ea + T::SIZE], T::truncate_from(reg as u64));
}

impl Cop for SpCop2 {
    fn reg(&self, idx: usize) -> u128 {
        match idx {
            SpCop2::REG_VCO => self.vco() as u128,
            SpCop2::REG_VCC => self.vcc() as u128,
            SpCop2::REG_VCE => self.vce() as u128,
            SpCop2::REG_ACCUM_LO => LittleEndian::read_u128(&self.accum[0].0),
            SpCop2::REG_ACCUM_MD => LittleEndian::read_u128(&self.accum[1].0),
            SpCop2::REG_ACCUM_HI => LittleEndian::read_u128(&self.accum[2].0),
            _ => LittleEndian::read_u128(&self.vregs.0[idx]),
        }
    }
    fn set_reg(&mut self, idx: usize, val: u128) {
        match idx {
            SpCop2::REG_VCO => self.set_vco(val as u16),
            SpCop2::REG_VCC => self.set_vcc(val as u16),
            SpCop2::REG_VCE => self.set_vce(val as u16),
            SpCop2::REG_ACCUM_LO => LittleEndian::write_u128(&mut self.accum[0].0, val),
            SpCop2::REG_ACCUM_MD => LittleEndian::write_u128(&mut self.accum[1].0, val),
            SpCop2::REG_ACCUM_HI => LittleEndian::write_u128(&mut self.accum[2].0, val),
            _ => LittleEndian::write_u128(&mut self.vregs.0[idx], val),
        }
    }

    fn op(&mut self, cpu: &mut CpuContext, op: u32) {
        unsafe { self.uop(cpu, op) }
    }

    fn lwc(&mut self, op: u32, ctx: &CpuContext, _bus: &Rc<RefCell<Box<Bus>>>) {
        let sp = self.sp.borrow();
        let dmem = sp.dmem.buf();
        let (base, vt, op, element, offset) = SpCop2::oploadstore(op, ctx);
        let regptr = &mut self.vregs.0[vt];
        match op {
            0x00 => lxv::<u8>(regptr, element as usize, &dmem, base, offset), // LBV
            0x01 => lxv::<u16>(regptr, element as usize, &dmem, base, offset), // LSV
            0x02 => lxv::<u32>(regptr, element as usize, &dmem, base, offset), // LLV
            0x03 => lxv::<u64>(regptr, element as usize, &dmem, base, offset), // LDV
            0x04 => {
                // LQV
                let ea = ((base + (offset << 4)) & 0xFFF) as usize;
                let qw_start = ea & !0xF;
                let ea_idx = ea & 0xF;

                let mut mem = BigEndian::read_u128(&dmem[qw_start..qw_start + 0x10]);
                mem <<= ea_idx * 8;

                let regptr = &mut self.vregs.0[vt];
                write_partial_right::<LittleEndian>(regptr, mem, element as usize * 8, 128);
            }
            0x05 => {
                // LRV
                let ea = ((base + (offset << 4)) & 0xFFF) as usize;
                let qw_start = ea & !0xF;
                let ea_idx = ea & 0xF;

                let mem = BigEndian::read_u128(&dmem[qw_start..qw_start + 0x10]);
                let sh = (16 - ea_idx) + element as usize;

                let regptr = &mut self.vregs.0[vt];
                write_partial_right::<LittleEndian>(regptr, mem, sh * 8, 128);
            }
            0x0B => {
                // LTV
                let ea = (base + (offset << 4)) & 0xFFF;
                let qw_start = ea as usize & !0x7;
                let mut mem = BigEndian::read_u128(&dmem[qw_start..qw_start + 0x10]);

                let mut e: usize = 7;
                let vtbase = vt & !7;
                let mut vtoff = element as usize >> 1;
                mem = mem.rotate_left((element + (ea & 0x8)) * 8);

                for _ in 0..8 {
                    LittleEndian::write_u16(
                        &mut self.vregs.0[vtbase + vtoff][e * 2..],
                        (mem >> (128 - 16)) as u16,
                    );
                    mem <<= 16;
                    e -= 1;
                    vtoff += 1;
                    vtoff &= 7;
                }
            }
            _ => panic!("unimplemented VU load opcode={}", op.hex()),
        }
    }
    fn swc(&mut self, op: u32, ctx: &CpuContext, _bus: &Rc<RefCell<Box<Bus>>>) {
        let sp = self.sp.borrow();
        let mut dmem = sp.dmem.buf();
        let (base, vt, op, element, offset) = SpCop2::oploadstore(op, ctx);
        let regptr = &self.vregs.0[vt];
        match op {
            0x00 => sxv::<u8>(&mut dmem, base, offset, regptr, element as usize), // SBV
            0x01 => sxv::<u16>(&mut dmem, base, offset, regptr, element as usize), // SSV
            0x02 => sxv::<u32>(&mut dmem, base, offset, regptr, element as usize), // SLV
            0x03 => sxv::<u64>(&mut dmem, base, offset, regptr, element as usize), // SDV
            0x04 => {
                // SQV
                let ea = ((base + (offset << 4)) & 0xFFF) as usize;
                let qw_start = ea & !0xF;
                let ea_idx = ea & 0xF;
                let regptr = &self.vregs.0[vt];

                let mut reg = LittleEndian::read_u128(regptr);
                reg = reg.rotate_left(element * 8);

                let memptr = &mut dmem[qw_start..qw_start + 0x10];
                write_partial_right::<BigEndian>(memptr, reg, ea_idx * 8, 128);
            }
            0x05 => {
                // SRV
                let ea = ((base + (offset << 4)) & 0xFFF) as usize;
                let qw_start = ea & !0xF;
                let ea_idx = ea & 0xF;
                let regptr = &self.vregs.0[vt];

                let mut reg = LittleEndian::read_u128(regptr);
                reg = reg.rotate_left(element * 8);

                let memptr = &mut dmem[qw_start..qw_start + 0x10];
                write_partial_left::<BigEndian>(memptr, reg, (16 - ea_idx) * 8);
            }
            0x0A => {
                // SWV
                let ea = (base + (offset << 4)) & 0xFFF;
                let qw_start = ea as usize & !0x7;

                let mut mem = LittleEndian::read_u128(regptr);
                mem = mem.rotate_right((ea & 7) * 8);
                mem = mem.rotate_left(element * 8);
                BigEndian::write_u128(&mut dmem[qw_start..qw_start + 0x10], mem);
            }
            0x0B => {
                // STV
                let ea = (base + (offset << 4)) & 0xFFF;
                let qw_start = ea as usize & !0x7;
                let mut mem: u128 = 0;

                let mut e: usize = 7;
                let vtbase = vt & !7;
                let mut vtoff = element as usize >> 1;

                for _ in 0..8 {
                    let r = LittleEndian::read_u16(&self.vregs.0[vtbase + vtoff][e * 2..]);
                    mem <<= 16;
                    mem |= r as u128;
                    e -= 1;
                    vtoff += 1;
                    vtoff &= 7;
                }

                mem = mem.rotate_right((ea & 7) * 8);
                BigEndian::write_u128(&mut dmem[qw_start..qw_start + 0x10], mem);
            }
            _ => panic!("unimplemented VU store opcode={}", op.hex()),
        }
    }

    fn ldc(&mut self, _op: u32, _ctx: &CpuContext, _bus: &Rc<RefCell<Box<Bus>>>) {
        unimplemented!()
    }
    fn sdc(&mut self, _op: u32, _ctx: &CpuContext, _bus: &Rc<RefCell<Box<Bus>>>) {
        unimplemented!()
    }
}
